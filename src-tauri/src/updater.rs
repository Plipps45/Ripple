use crate::commands::emit_main;
use crate::{logger, window};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tauri::AppHandle;
use tokio::io::AsyncWriteExt;

const LATEST_RELEASE: &str = "https://api.github.com/repos/Plipps45/Ripple/releases/latest";

static PENDING_URL: Mutex<Option<String>> = Mutex::new(None);
static INSTALL_ON_EXIT: Mutex<Option<PathBuf>> = Mutex::new(None);

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("Ripple-Updater")
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(err)
}

fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().trim_start_matches(['v', 'V']).split(['-', '+']).next()?;
    let n: Vec<u64> = core.split('.').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    Some((n[0], *n.get(1).unwrap_or(&0), *n.get(2).unwrap_or(&0)))
}

async fn fetch_latest() -> Result<Value, String> {
    let res = client()?
        .get(LATEST_RELEASE)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(err)?
        .error_for_status()
        .map_err(err)?;
    serde_json::from_str(&res.text().await.map_err(err)?).map_err(err)
}

pub fn check(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let rel = match fetch_latest().await {
            Ok(v) => v,
            Err(e) => return emit_main(&app, "update-error", json!({ "message": e })),
        };
        let current = app.package_info().version.to_string();
        let tag = rel["tag_name"].as_str().unwrap_or("");
        let newer = matches!((parse_version(tag), parse_version(&current)), (Some(l), Some(c)) if l > c);
        if !newer {
            return emit_main(&app, "update-not-available", json!({ "version": current }));
        }
        let installer = rel["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|a| a["name"].as_str().is_some_and(|n| n.ends_with("-setup.exe")))
            .and_then(|a| a["browser_download_url"].as_str())
            .map(String::from);
        // Dev builds would install the released app over nothing, so they only get the release link.
        let can_install = cfg!(windows) && !cfg!(debug_assertions) && installer.is_some();
        *PENDING_URL.lock().unwrap() = installer;
        emit_main(&app, "update-available", json!({
            "version": tag.trim_start_matches(['v', 'V']),
            "canInstall": can_install,
            "url": rel["html_url"],
        }));
    });
}

pub fn action(app: AppHandle, action: &str) {
    let now = match action {
        "now" => true,
        "later" => false,
        _ => return,
    };
    let Some(url) = PENDING_URL.lock().unwrap().take() else { return };
    tauri::async_runtime::spawn(async move {
        let path = match download(&app, &url).await {
            Ok(p) => p,
            Err(e) => return emit_main(&app, "update-error", json!({ "message": e })),
        };
        if !now {
            *INSTALL_ON_EXIT.lock().unwrap() = Some(path);
            return emit_main(&app, "update-ready", json!({}));
        }
        match run_installer(&path, true) {
            Ok(()) => window::quit_app(&app),
            Err(e) => emit_main(&app, "update-error", json!({ "message": e.to_string() })),
        }
    });
}

async fn download(app: &AppHandle, url: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("ripple-update");
    tokio::fs::create_dir_all(&dir).await.map_err(err)?;
    let path = dir.join("ripple-setup.exe");
    let res = client()?.get(url).send().await.map_err(err)?.error_for_status().map_err(err)?;
    let total = res.content_length().unwrap_or(0);
    let mut file = tokio::fs::File::create(&path).await.map_err(err)?;
    let mut stream = res.bytes_stream();
    let (mut got, mut last_pct) = (0u64, u64::MAX);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(err)?;
        file.write_all(&chunk).await.map_err(err)?;
        got += chunk.len() as u64;
        let pct = if total > 0 { got * 100 / total } else { 0 };
        if pct != last_pct {
            last_pct = pct;
            emit_main(app, "update-progress", json!({ "percent": pct }));
        }
    }
    file.flush().await.map_err(err)?;
    if total > 0 && got != total {
        return Err("download incomplete".into());
    }
    Ok(path)
}

// Tauri NSIS flags: /UPDATE reuses the existing install, /P shows progress only, /S is silent, /R relaunches Ripple.
fn run_installer(path: &Path, relaunch: bool) -> std::io::Result<()> {
    let args: &[&str] = if relaunch { &["/UPDATE", "/P", "/R"] } else { &["/UPDATE", "/S"] };
    std::process::Command::new(path).args(args).spawn().map(|_| ())
}

pub fn install_on_exit() {
    if let Some(path) = INSTALL_ON_EXIT.lock().unwrap().take() {
        if let Err(e) = run_installer(&path, false) {
            logger::error("Update install failed", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn compares_versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("v2.0.0-beta.1"), Some((2, 0, 0)));
        assert_eq!(parse_version("latest"), None);
        assert!(parse_version("v1.0.10") > parse_version("1.0.9"));
        assert!(parse_version("v1.0.0") == parse_version("1.0.0"));
    }
}
