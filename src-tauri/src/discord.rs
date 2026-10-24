use serde_json::{json, Value};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const CLIENT_ID: &str = "1562580262192160789";
const RECONNECT_DELAY: Duration = Duration::from_secs(15);

enum Cmd {
    Update(Value),
    Clear,
    SetEnabled(bool),
    Reconnect,
    Shutdown,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Connected,
    NoClientId,
    DiscordNotRunning,
    Rejected,
    Disabled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Connected => "connected",
            Status::NoClientId => "no-client-id",
            Status::DiscordNotRunning => "discord-not-running",
            Status::Rejected => "rejected",
            Status::Disabled => "disabled",
        }
    }
}

static CONFIGURED_ID: Mutex<String> = Mutex::new(String::new());

pub fn valid_client_id(id: &str) -> bool {
    (17..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

pub struct Discord {
    tx: Sender<Cmd>,
    enabled: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
}

static INSTANCE: OnceLock<Discord> = OnceLock::new();

pub fn get() -> &'static Discord {
    INSTANCE.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        let enabled = Arc::new(AtomicBool::new(true));
        let connected = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(if client_id().is_some() { Status::DiscordNotRunning } else { Status::NoClientId }));
        let (e, c, st) = (enabled.clone(), connected.clone(), status.clone());
        std::thread::Builder::new().name("discord-rpc".into()).spawn(move || worker(rx, e, c, st)).ok();
        Discord { tx, enabled, connected, status }
    })
}

impl Discord {
    pub fn update(&self, info: Value) {
        let _ = self.tx.send(Cmd::Update(info));
    }
    pub fn clear(&self) {
        let _ = self.tx.send(Cmd::Clear);
    }
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
        let _ = self.tx.send(Cmd::SetEnabled(on));
    }
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }
    pub fn status(&self) -> Status {
        if !self.enabled() {
            return Status::Disabled;
        }
        *self.status.lock().unwrap()
    }
    pub fn set_client_id(&self, id: &str) {
        *CONFIGURED_ID.lock().unwrap() = id.trim().to_string();
        let _ = self.tx.send(Cmd::Reconnect);
    }
    pub fn shutdown(&self) {
        let _ = self.tx.send(Cmd::Shutdown);
    }
}

#[cfg(windows)]
type Pipe = std::fs::File;
#[cfg(unix)]
type Pipe = std::os::unix::net::UnixStream;

#[cfg(windows)]
fn open_pipe() -> Option<Pipe> {
    (0..10).find_map(|i| std::fs::OpenOptions::new().read(true).write(true).open(format!(r"\\.\pipe\discord-ipc-{i}")).ok())
}

#[cfg(unix)]
fn open_pipe() -> Option<Pipe> {
    let mut bases: Vec<std::path::PathBuf> = ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"]
        .iter()
        .filter_map(|k| std::env::var_os(k).map(std::path::PathBuf::from))
        .collect();
    bases.push("/tmp".into());
    for base in bases {
        for sub in ["", "app/com.discordapp.Discord", "snap.discord"] {
            for i in 0..10 {
                let p = base.join(sub).join(format!("discord-ipc-{i}"));
                if let Ok(s) = Pipe::connect(&p) {
                    return Some(s);
                }
            }
        }
    }
    None
}

fn send_frame(pipe: &mut Pipe, op: u32, payload: &Value) -> std::io::Result<()> {
    let data = payload.to_string();
    let mut buf = Vec::with_capacity(8 + data.len());
    buf.extend_from_slice(&op.to_le_bytes());
    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
    buf.extend_from_slice(data.as_bytes());
    pipe.write_all(&buf)?;
    pipe.flush()
}

fn read_frame(pipe: &mut Pipe) -> std::io::Result<(u32, Value)> {
    let mut header = [0u8; 8];
    pipe.read_exact(&mut header)?;
    let op = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    let mut body = vec![0u8; len.min(1 << 20)];
    pipe.read_exact(&mut body)?;
    Ok((op, serde_json::from_slice(&body).unwrap_or(Value::Null)))
}

pub fn client_id() -> Option<String> {
    let configured = CONFIGURED_ID.lock().unwrap().clone();
    std::env::var("RIPPLE_DISCORD_CLIENT_ID")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| (!configured.is_empty()).then_some(configured))
        .or_else(|| (!CLIENT_ID.is_empty()).then(|| CLIENT_ID.to_string()))
}

fn connect() -> Result<Pipe, Status> {
    let id = client_id().ok_or(Status::NoClientId)?;
    let mut pipe = open_pipe().ok_or(Status::DiscordNotRunning)?;
    send_frame(&mut pipe, 0, &json!({ "v": 1, "client_id": id })).map_err(|_| Status::DiscordNotRunning)?;
    let (op, msg) = read_frame(&mut pipe).map_err(|_| Status::Rejected)?;
    if op == 1 && msg.get("evt").and_then(Value::as_str) == Some("READY") {
        Ok(pipe)
    } else {
        crate::logger::log("warn", &format!("[discord] handshake refused: {msg}"));
        Err(Status::Rejected)
    }
}

fn nonce() -> String {
    format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0))
}

fn build_activity(info: &Value) -> Value {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let start = info.get("startTimestamp").and_then(Value::as_i64).filter(|v| *v != 0).unwrap_or(now);
    let text = |k: &str, fallback: &str| -> String {
        info.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(fallback).chars().take(128).collect()
    };
    let mut activity = json!({
        "type": 2,
        "details": text("title", "Unknown Track"),
        "state": text("artist", "Unknown Artist"),
        "status_display_type": 1,
        "timestamps": { "start": start },
        "instance": false
    });
    if let Some(thumb) = info.get("thumbnail").and_then(Value::as_str).filter(|t| !t.is_empty()) {
        activity["assets"] = json!({ "large_image": thumb });
    }
    if let Some(d) = info.get("duration").and_then(Value::as_f64).filter(|d| *d > 0.0) {
        activity["timestamps"]["end"] = json!(start + d.floor() as i64);
    }
    if let Some(url) = info.get("url").and_then(Value::as_str).filter(|u| u.starts_with("http")) {
        let label = if info.get("platform").and_then(Value::as_str) == Some("soundcloud") { "Listen on SoundCloud" } else { "Listen on YouTube" };
        activity["buttons"] = json!([{ "label": label, "url": url }]);
    }
    activity
}

fn set_activity(pipe: &mut Pipe, activity: Value) -> std::io::Result<()> {
    let payload = json!({ "cmd": "SET_ACTIVITY", "args": { "pid": std::process::id(), "activity": activity }, "nonce": nonce() });
    send_frame(pipe, 1, &payload)?;
    read_frame(pipe).map(|_| ())
}

fn worker(rx: mpsc::Receiver<Cmd>, enabled: Arc<AtomicBool>, connected: Arc<AtomicBool>, status: Arc<Mutex<Status>>) {
    let mut pipe: Option<Pipe> = None;
    let mut last: Option<Value> = None;
    let mut next_attempt = Instant::now();
    loop {
        if pipe.is_none() && enabled.load(Ordering::SeqCst) && Instant::now() >= next_attempt {
            let attempt = connect();
            *status.lock().unwrap() = attempt.as_ref().err().copied().unwrap_or(Status::Connected);
            pipe = attempt.ok();
            connected.store(pipe.is_some(), Ordering::SeqCst);
            next_attempt = Instant::now() + RECONNECT_DELAY;
            if let (Some(p), Some(info)) = (pipe.as_mut(), last.as_ref()) {
                if set_activity(p, build_activity(info)).is_err() {
                    pipe = None;
                    connected.store(false, Ordering::SeqCst);
                }
            }
        }
        let cmd = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(c) => c,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let result = match cmd {
            Cmd::Update(info) => {
                last = Some(info.clone());
                match (enabled.load(Ordering::SeqCst), pipe.as_mut()) {
                    (true, Some(p)) => set_activity(p, build_activity(&info)),
                    _ => Ok(()),
                }
            }
            Cmd::Clear => {
                last = None;
                match pipe.as_mut() {
                    Some(p) => set_activity(p, Value::Null),
                    None => Ok(()),
                }
            }
            Cmd::SetEnabled(on) => {
                if on {
                    next_attempt = Instant::now();
                    match (pipe.as_mut(), last.as_ref()) {
                        (Some(p), Some(info)) => set_activity(p, build_activity(info)),
                        _ => Ok(()),
                    }
                } else {
                    if let Some(p) = pipe.as_mut() {
                        let _ = set_activity(p, Value::Null);
                    }
                    pipe = None;
                    connected.store(false, Ordering::SeqCst);
                    Ok(())
                }
            }
            Cmd::Reconnect => {
                if let Some(p) = pipe.as_mut() {
                    let _ = set_activity(p, Value::Null);
                }
                pipe = None;
                connected.store(false, Ordering::SeqCst);
                *status.lock().unwrap() = if client_id().is_some() { Status::DiscordNotRunning } else { Status::NoClientId };
                next_attempt = Instant::now();
                Ok(())
            }
            Cmd::Shutdown => {
                if let Some(p) = pipe.as_mut() {
                    let _ = set_activity(p, Value::Null);
                }
                break;
            }
        };
        if result.is_err() {
            pipe = None;
            connected.store(false, Ordering::SeqCst);
            *status.lock().unwrap() = Status::DiscordNotRunning;
            next_attempt = Instant::now() + RECONNECT_DELAY;
        }
    }
    connected.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ids_are_snowflakes() {
        assert!(valid_client_id("1234567890123456789"));
        assert!(!valid_client_id("12345"));
        assert!(!valid_client_id("12345678901234567a9"));
        assert!(!valid_client_id(""));
    }
}
