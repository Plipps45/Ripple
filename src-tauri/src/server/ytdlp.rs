use crate::logger;
use crate::paths::{self, Paths};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn command(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    cmd.stdin(Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

pub fn strs(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

pub async fn execute(paths: &Paths, args: Vec<String>, timeout_ms: u64) -> Result<String, String> {
    run_yt_dlp(paths, args, timeout_ms, false).await
}

pub async fn execute_partial(paths: &Paths, args: Vec<String>, timeout_ms: u64) -> Result<String, String> {
    run_yt_dlp(paths, args, timeout_ms, true).await
}

async fn run_yt_dlp(paths: &Paths, args: Vec<String>, timeout_ms: u64, keep_partial: bool) -> Result<String, String> {
    let (bin, full) = paths::yt_dlp_command(paths, &args)?;
    let first_args = args.iter().take(3).cloned().collect::<Vec<_>>().join(" ");
    let mut cmd = command(&bin);
    cmd.args(&full).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd.spawn().map_err(|e| {
        logger::log("error", &format!("yt-dlp spawn error: {first_args} - {}", truncate(&e.to_string(), 300)));
        e.to_string()
    })?;
    let output = if timeout_ms > 0 {
        match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output()).await {
            Ok(r) => r.map_err(|e| e.to_string())?,
            Err(_) => return Err(format!("yt-dlp timed out after {timeout_ms}ms")),
        }
    } else {
        child.wait_with_output().await.map_err(|e| e.to_string())?
    };
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() || (keep_partial && !stdout.trim().is_empty()) {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let code = output.status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
        let snippet = if stderr.is_empty() { format!("code {code}") } else { truncate(&stderr, 300) };
        logger::log("error", &format!("yt-dlp failed: {first_args} - {snippet}"));
        Err(if stderr.is_empty() { format!("yt-dlp exited with code {code}") } else { stderr })
    }
}

pub async fn run(bin: &Path, args: &[String], timeout_ms: u64) -> Result<(), String> {
    let mut cmd = command(bin);
    cmd.args(args).stdout(Stdio::null()).stderr(Stdio::piped());
    let child = cmd.spawn().map_err(|e| e.to_string())?;
    let name = bin.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let out = match tokio::time::timeout(Duration::from_millis(timeout_ms.max(1)), child.wait_with_output()).await {
        Ok(r) => r.map_err(|e| e.to_string())?,
        Err(_) => return Err(format!("{name} timed out")),
    };
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let code = out.status.code().unwrap_or(-1);
        if stderr.trim().is_empty() {
            Err(format!("{name} exited with code {code}"))
        } else {
            Err(stderr)
        }
    }
}

pub fn first_http_line(out: &str) -> Option<String> {
    out.trim().lines().map(str::trim).find(|l| l.starts_with("http")).map(str::to_string)
}

pub fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}
