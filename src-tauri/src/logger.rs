use parking_lot::Mutex;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

const MAX_BYTES: u64 = 1024 * 1024;

static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();
static WRITE_LOCK: Mutex<()> = Mutex::new(());

pub fn init(dir: PathBuf) {
    let _ = LOG_DIR.set(dir);
}

pub fn log_dir() -> PathBuf {
    LOG_DIR.get().cloned().unwrap_or_else(|| PathBuf::from("logs"))
}

fn log_path() -> PathBuf {
    log_dir().join("ripple.log")
}

pub fn log(level: &str, message: &str) {
    let _guard = WRITE_LOCK.lock();
    let path = log_path();
    let _ = fs::create_dir_all(log_dir());
    if fs::metadata(&path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let mut rotated = path.clone().into_os_string();
        rotated.push(".1");
        let _ = fs::rename(&path, rotated);
    }
    let msg: String = message.chars().take(800).collect();
    let stamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{stamp} [{level}] {msg}");
    }
}

pub fn error(context: &str, err: impl std::fmt::Display) {
    log("ERROR", &format!("{context}: {err}"));
}

pub fn tail(lines: usize) -> Vec<String> {
    let text = fs::read_to_string(log_path()).unwrap_or_default();
    let all: Vec<&str> = text.split('\n').filter(|l| !l.is_empty()).collect();
    let start = all.len().saturating_sub(lines);
    all[start..].iter().map(|s| s.to_string()).collect()
}
