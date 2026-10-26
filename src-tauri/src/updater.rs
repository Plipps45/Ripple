use crate::commands::emit_main;
use crate::window;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tauri::AppHandle;

const LATEST_RELEASE: &str = "https://api.github.com/repos/Plipps45/Ripple/releases/latest";

static CHECKING: AtomicBool = AtomicBool::new(false);
static READY: Mutex<Option<(String, PathBuf)>> = Mutex::new(None);

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
    if CHECKING.swap(true, Ordering::SeqCst) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        if let Err(e) = find_and_download(&app).await {
            emit_main(&app, "update-error", json!({ "message": e }));
        }
        CHECKING.store(false, Ordering::SeqCst);
    });
}

async fn find_and_download(app: &AppHandle) -> Result<(), String> {
    if let Some((version, _)) = READY.lock().unwrap().clone() {
        emit_main(app, "update-ready", json!({ "version": version }));
        return Ok(());
    }
    let rel = fetch_latest().await?;
    let current = app.package_info().version.to_string();
    let latest = match (parse_version(rel["tag_name"].as_str().unwrap_or("")), parse_version(&current)) {
        (Some(l), Some(c)) if l > c => l,
        _ => {
            emit_main(app, "update-not-available", json!({ "version": current }));
            return Ok(());
        }
    };
    let version = format!("{}.{}.{}", latest.0, latest.1, latest.2);
    let installer = rel["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str().is_some_and(|n| n.ends_with("-setup.exe")));
    // Dev builds would install the released app over nothing, so they only get the release link.
    let installer = match installer {
        Some(a) if cfg!(windows) && !cfg!(debug_assertions) => a,
        _ => {
            emit_main(app, "update-available", json!({ "version": version, "url": rel["html_url"] }));
            return Ok(());
        }
    };
    let path = download(&version, installer).await?;
    *READY.lock().unwrap() = Some((version.clone(), path));
    emit_main(app, "update-ready", json!({ "version": version }));
    Ok(())
}

fn verified(bytes: &[u8], size: u64, sha256: Option<&str>) -> bool {
    (size == 0 || bytes.len() as u64 == size) && sha256.map_or(true, |d| hex::encode(Sha256::digest(bytes)) == d)
}

async fn download(version: &str, asset: &Value) -> Result<PathBuf, String> {
    let url = asset["browser_download_url"].as_str().ok_or("release has no installer url")?;
    let size = asset["size"].as_u64().unwrap_or(0);
    let sha256 = asset["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).map(str::to_ascii_lowercase);
    let dir = std::env::temp_dir().join("ripple-update");
    tokio::fs::create_dir_all(&dir).await.map_err(err)?;
    let path = dir.join(format!("ripple-{version}-setup.exe"));
    if tokio::fs::read(&path).await.is_ok_and(|b| verified(&b, size, sha256.as_deref())) {
        return Ok(path);
    }
    let bytes = client()?
        .get(url)
        .send()
        .await
        .map_err(err)?
        .error_for_status()
        .map_err(err)?
        .bytes()
        .await
        .map_err(err)?;
    if !verified(&bytes, size, sha256.as_deref()) {
        return Err("downloaded installer failed verification".into());
    }
    tokio::fs::write(&path, &bytes).await.map_err(err)?;
    Ok(path)
}

pub fn action(app: AppHandle, action: &str) {
    if action != "now" {
        return;
    }
    let Some(path) = READY.lock().unwrap().as_ref().map(|r| r.1.clone()) else { return };
    match run_installer(&path) {
        Ok(()) => window::quit_app(&app),
        Err(e) => emit_main(&app, "update-error", json!({ "message": e.to_string() })),
    }
}

// Tauri NSIS flags: /UPDATE reuses the existing install, /S hides the installer, /R relaunches Ripple.
fn run_installer(path: &Path) -> std::io::Result<()> {
    std::process::Command::new(path).args(["/UPDATE", "/S", "/R"]).spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::{parse_version, verified};

    #[test]
    fn compares_versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("v2.0.0-beta.1"), Some((2, 0, 0)));
        assert_eq!(parse_version("latest"), None);
        assert!(parse_version("v1.0.10") > parse_version("1.0.9"));
        assert!(parse_version("v1.0.0") == parse_version("1.0.0"));
    }

    #[test]
    fn verifies_installer() {
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verified(b"abc", 3, Some(abc)));
        assert!(verified(b"abc", 0, None));
        assert!(!verified(b"abc", 4, Some(abc)));
        assert!(!verified(b"abd", 3, Some(abc)));
    }
}
