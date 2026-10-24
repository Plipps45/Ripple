use super::{ytdlp, SharedCtx};
use crate::logger;
use crate::paths::{self, IS_WIN};
use futures_util::StreamExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

fn yt_dlp_asset() -> &'static str {
    let arch = std::env::consts::ARCH;
    if IS_WIN {
        match arch {
            "aarch64" => "yt-dlp_arm64.exe",
            "x86" => "yt-dlp_x86.exe",
            _ => "yt-dlp.exe",
        }
    } else if cfg!(target_os = "macos") {
        "yt-dlp_macos"
    } else if arch == "aarch64" {
        "yt-dlp_linux_aarch64"
    } else {
        "yt-dlp_linux"
    }
}

fn ffmpeg_asset() -> Option<String> {
    let a = if std::env::consts::ARCH == "aarch64" { "arm64" } else { "64" };
    if IS_WIN {
        Some(format!("ffmpeg-master-latest-win{a}-gpl.zip"))
    } else if cfg!(target_os = "linux") {
        Some(format!("ffmpeg-master-latest-linux{a}-gpl.tar.xz"))
    } else {
        None
    }
}

pub fn expected_hash(sums: &str, file_name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let line = line.trim();
        let (hash, rest) = line.split_once(char::is_whitespace)?;
        let name = rest.trim().trim_start_matches('*').trim_start_matches("./").trim();
        (hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) && name == file_name).then(|| hash.to_lowercase())
    })
}

fn set_progress(ctx: &SharedCtx, f: impl FnOnce(&mut super::ToolsProgress)) {
    f(&mut ctx.tools.lock());
}

async fn get_text(ctx: &SharedCtx, url: &str) -> Result<String, String> {
    let resp = ctx.http.get(url).header("User-Agent", "Ripple").timeout(Duration::from_secs(30)).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} for {url}", resp.status().as_u16()));
    }
    resp.text().await.map_err(|e| e.to_string())
}

async fn download_file(ctx: &SharedCtx, url: &str, dest: &Path) -> Result<String, String> {
    let resp = ctx.http.get(url).header("User-Agent", "Ripple").send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} for {url}", resp.status().as_u16()));
    }
    let total = resp.content_length().unwrap_or(0);
    let mut file = tokio::fs::File::create(dest).await.map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut done: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        hasher.update(&chunk);
        done += chunk.len() as u64;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        if total > 0 {
            let pct = ((done as f64 / total as f64) * 100.0).round() as u32;
            set_progress(ctx, |p| p.percent = pct);
        }
    }
    file.flush().await.map_err(|e| e.to_string())?;
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(unix)]
fn make_executable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755));
}

#[cfg(not(unix))]
fn make_executable(_: &Path) {}

async fn install_yt_dlp(ctx: &SharedCtx) -> Result<(), String> {
    let asset = yt_dlp_asset();
    let base = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";
    let sums = get_text(ctx, &format!("{base}/SHA2-256SUMS")).await?;
    let want = expected_hash(&sums, asset).ok_or_else(|| format!("No checksum published for {asset}"))?;
    let target = ctx.paths.bin_dir.join(if IS_WIN { "yt-dlp.exe" } else { "yt-dlp" });
    let tmp = target.with_extension("download");
    let got = download_file(ctx, &format!("{base}/{asset}"), &tmp).await?;
    if got != want {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err("yt-dlp checksum mismatch".into());
    }
    make_executable(&tmp);
    tokio::fs::rename(&tmp, &target).await.map_err(|e| e.to_string())
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Some(hit) = find_file(&p, name) {
                return Some(hit);
            }
        } else if entry.file_name() == name {
            return Some(p);
        }
    }
    None
}

async fn install_ffmpeg(ctx: &SharedCtx) -> Result<(), String> {
    let asset = ffmpeg_asset().ok_or("No ffmpeg build for this system. Install ffmpeg with your package manager (for macOS: brew install ffmpeg).")?;
    let base = "https://github.com/yt-dlp/FFmpeg-Builds/releases/latest/download";
    let sums = get_text(ctx, &format!("{base}/checksums.sha256")).await?;
    let want = expected_hash(&sums, &asset).ok_or_else(|| format!("No checksum published for {asset}"))?;
    let archive = std::env::temp_dir().join(format!("ripple-{asset}"));
    let got = download_file(ctx, &format!("{base}/{asset}"), &archive).await?;
    if got != want {
        let _ = tokio::fs::remove_file(&archive).await;
        return Err("ffmpeg checksum mismatch".into());
    }
    let work = std::env::temp_dir().join(format!("ripple-ffmpeg-{}", super::textutil::random_base36(8)));
    let _ = std::fs::create_dir_all(&work);
    let result = async {
        let args = vec!["-xf".to_string(), archive.to_string_lossy().into_owned(), "-C".into(), work.to_string_lossy().into_owned()];
        ytdlp::run(Path::new("tar"), &args, 600_000).await?;
        for name in ["ffmpeg", "ffprobe"] {
            let exe = if IS_WIN { format!("{name}.exe") } else { name.to_string() };
            let src = find_file(&work, &exe).ok_or_else(|| format!("{exe} not found inside the archive"))?;
            let dest = ctx.paths.bin_dir.join(&exe);
            std::fs::copy(&src, &dest).map_err(|e| e.to_string())?;
            make_executable(&dest);
        }
        Ok::<_, String>(())
    }
    .await;
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(&archive);
    result
}

pub fn tools_status(ctx: &SharedCtx) -> Value {
    json!({
        "ytDlp": paths::resolve_binary(&ctx.paths, "yt-dlp").is_some(),
        "ffmpeg": paths::resolve_binary(&ctx.paths, "ffmpeg").is_some(),
        "progress": ctx.tools.lock().clone()
    })
}

pub async fn ensure_tools(ctx: SharedCtx) -> Value {
    if ctx.tools.lock().status == "downloading" {
        return tools_status(&ctx);
    }
    let _ = std::fs::create_dir_all(&ctx.paths.bin_dir);
    set_progress(&ctx, |p| {
        p.status = "downloading".into();
        p.error.clear();
    });
    let result = async {
        if paths::resolve_binary(&ctx.paths, "yt-dlp").is_none() {
            set_progress(&ctx, |p| {
                p.tool = "yt-dlp".into();
                p.percent = 0;
            });
            install_yt_dlp(&ctx).await?;
            paths::clear_binary_cache();
        }
        if paths::resolve_binary(&ctx.paths, "ffmpeg").is_none() {
            set_progress(&ctx, |p| {
                p.tool = "ffmpeg".into();
                p.percent = 0;
            });
            install_ffmpeg(&ctx).await?;
            paths::clear_binary_cache();
        }
        Ok::<_, String>(())
    }
    .await;
    match result {
        Ok(()) => set_progress(&ctx, |p| p.status = "done".into()),
        Err(e) => {
            let tool = ctx.tools.lock().tool.clone();
            logger::error(&format!("Tool download failed: {tool}"), &e);
            set_progress(&ctx, |p| {
                p.status = "error".into();
                p.error = e;
            });
        }
    }
    tools_status(&ctx)
}

pub async fn update_yt_dlp_if_managed(ctx: &SharedCtx) {
    let Some(bin) = paths::resolve_binary(&ctx.paths, "yt-dlp") else { return };
    if bin.parent() != Some(ctx.paths.bin_dir.as_path()) {
        return;
    }
    let stamp = ctx.paths.bin_dir.join(".ytdlp-update-stamp");
    let last: u64 = std::fs::read_to_string(&stamp).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    if super::textutil::now_ms().saturating_sub(last) < 24 * 3600 * 1000 {
        return;
    }
    match ytdlp::run(&bin, &["-U".to_string()], 120_000).await {
        Ok(()) => {
            let _ = std::fs::write(&stamp, super::textutil::now_ms().to_string());
        }
        Err(e) => logger::error("yt-dlp update failed", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_lines() {
        let sums = format!("{}  yt-dlp.exe\n{} *./other.zip\n", "a".repeat(64), "B".repeat(64));
        assert_eq!(expected_hash(&sums, "yt-dlp.exe"), Some("a".repeat(64)));
        assert_eq!(expected_hash(&sums, "other.zip"), Some("b".repeat(64)));
        assert_eq!(expected_hash(&sums, "missing"), None);
    }
}
