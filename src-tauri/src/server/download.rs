use super::media::{downloads_dir, ext_of, find_cached_file, AUDIO_EXTENSIONS};
use super::textutil::*;
use super::{json_response, ytdlp, Req, SharedCtx};
use crate::{logger, paths};
use axum::body::Body;
use axum::http::header;
use axum::response::Response;
use futures_util::StreamExt;
use regex::Regex;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

const DOWNLOADS_INDEX_TTL: Duration = Duration::from_secs(15);
const VALID_FORMATS: [&str; 6] = ["mp3", "m4a", "flac", "opus", "wav", "ogg"];
const MAX_ENCODE_BODY: usize = 500 * 1024 * 1024;
const DOWNLOAD_TIMEOUT_MS: u64 = 180_000;

static VIDEO_IN_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:v=|/embed/|/watch/|youtu\.be/|/v/)([a-zA-Z0-9_-]{11})").unwrap());
static WAV_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_ .-]+\.wav$").unwrap());

struct AbortGuard<F: FnOnce() + Send + 'static> {
    cleanup: Option<F>,
}

impl<F: FnOnce() + Send + 'static> AbortGuard<F> {
    fn new(f: F) -> Self {
        AbortGuard { cleanup: Some(f) }
    }
    fn disarm(&mut self) {
        self.cleanup = None;
    }
}

impl<F: FnOnce() + Send + 'static> Drop for AbortGuard<F> {
    fn drop(&mut self) {
        if let Some(f) = self.cleanup.take() {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(400));
                f();
            });
        }
    }
}

fn fmt_num(n: f64) -> String {
    js_string(&jnum(n))
}

fn bitrate_for(quality: &str) -> &'static str {
    match quality {
        "2" => "256k",
        "4" => "192k",
        "5" => "128k",
        _ => "320k",
    }
}

fn parse_time(v: Option<&String>) -> Option<f64> {
    v.filter(|s| !s.is_empty()).and_then(|s| s.trim().parse::<f64>().ok()).filter(|n| !n.is_nan()).map(|n| n.max(0.0))
}

async fn list_names(dir: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Ok(mut rd) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            out.insert(e.file_name().to_string_lossy().into_owned());
        }
    }
    out
}

fn list_names_sync(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default()
}

async fn build_downloads_index(ctx: &SharedCtx) -> HashMap<String, String> {
    let dir = downloads_dir();
    let mut map = HashMap::new();
    if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            if !e.file_type().await.map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let ext = ext_of(Path::new(&name));
            if !AUDIO_EXTENSIONS.contains(&ext.as_str()) {
                continue;
            }
            let base = name[..name.len() - ext.len()].to_lowercase();
            if !base.is_empty() {
                map.insert(base, name);
            }
        }
    }
    ctx.caches.lock().downloads_index = Some((Instant::now(), map.clone()));
    map
}

fn invalidate_downloads_index(ctx: &SharedCtx) {
    ctx.caches.lock().downloads_index = None;
}

pub async fn find_downloaded_file(ctx: &SharedCtx, title: &str, artist: &str) -> Option<String> {
    if title.is_empty() && artist.is_empty() {
        return None;
    }
    let fresh = ctx.caches.lock().downloads_index.as_ref().filter(|(at, _)| at.elapsed() < DOWNLOADS_INDEX_TTL).map(|(_, m)| m.clone());
    let index = match fresh {
        Some(m) => m,
        None => build_downloads_index(ctx).await,
    };
    let key = format!(
        "{} - {}",
        sanitize_filename(if artist.is_empty() { "unknown artist" } else { artist }),
        sanitize_filename(if title.is_empty() { "untitled" } else { title })
    )
    .to_lowercase();
    index.get(&key).cloned()
}

async fn cleanup_download_artifacts(dir: &Path, before: &HashSet<String>, base: &str, video_id: &str, ext: &str, keep_success: bool) {
    let trash = [".webp", ".webm", ".part", ".ytdl", ".temp", ".jpg", ".jpeg", ".png", ".json", ".vtt", ".m4a", ".opus", ".mp4"];
    let chosen = format!(".{}", ext.to_lowercase());
    let success_name = format!("{base}{chosen}");
    for f in list_names(dir).await {
        if before.contains(&f) {
            continue;
        }
        let fext = ext_of(Path::new(&f));
        let belongs = f.starts_with(base) || (!video_id.is_empty() && f.contains(video_id));
        if !belongs || (keep_success && f == success_name) {
            continue;
        }
        if trash.contains(&fext.as_str()) || !keep_success {
            if fext == chosen && keep_success {
                continue;
            }
            let _ = tokio::fs::remove_file(dir.join(&f)).await;
        }
    }
}

fn download_ok(raw: bool, temp_name: &str, out_filename: &str, dir: &Path) -> Response {
    json_response(200, &if raw {
        json!({ "success": true, "tempName": temp_name })
    } else {
        json!({ "success": true, "filename": out_filename, "directory": dir.to_string_lossy() })
    })
}

pub async fn handle_download(ctx: SharedCtx, req: Req) -> Response {
    let track_url = req.q("url");
    let track_id = req.q("id");
    let title = req.q("title");
    let artist = req.q("artist");
    let format = req.query.get("format").cloned().filter(|f| !f.is_empty()).unwrap_or_else(|| "mp3".into());
    let quality = req.query.get("quality").cloned().filter(|q| !q.is_empty()).unwrap_or_else(|| "0".into());
    let raw = matches!(req.q("raw").as_str(), "1" | "true");
    let start = parse_time(req.query.get("start"));
    let end = parse_time(req.query.get("end"));

    if track_url.is_empty() && (title.is_empty() || artist.is_empty()) {
        return json_response(400, &json!({ "success": false, "error": "missing track information" }));
    }

    let mut trim_args = Vec::new();
    if start.is_some() || end.is_some() {
        let s0 = start.unwrap_or(0.0);
        let to = end.filter(|e| *e != 0.0).map(|e| format!(" -to {}", fmt_num(e))).unwrap_or_default();
        trim_args.push("--postprocessor-args".to_string());
        trim_args.push(format!("ExtractAudio:-ss {}{to}", fmt_num(s0)));
    }

    let downloads = downloads_dir();
    let tmp_dir = downloads.join(".tmp");
    if raw && !tmp_dir.exists() {
        let _ = tokio::fs::create_dir_all(&tmp_dir).await;
    }
    let clean_title = sanitize_filename(if title.is_empty() { "untitled" } else { &title });
    let clean_artist = sanitize_filename(if artist.is_empty() { "unknown artist" } else { &artist });
    let safe_format = format.to_lowercase();
    let ext = if raw { "wav".to_string() } else if VALID_FORMATS.contains(&safe_format.as_str()) { safe_format } else { "mp3".into() };
    let safe_quality = if !quality.is_empty() && quality.chars().all(|c| c.is_ascii_digit()) { quality } else { "0".into() };
    let temp_name = if raw { format!("raw_{}_{}.wav", now_ms(), random_base36(6)) } else { String::new() };
    let out_filename = if raw { temp_name.clone() } else { format!("{clean_artist} - {clean_title}.{ext}") };
    let out_path = if raw { tmp_dir.join(&temp_name) } else { downloads.join(&out_filename) };
    let temp_template = if raw {
        tmp_dir.join(temp_name.trim_end_matches(".wav").to_string() + ".%(ext)s")
    } else {
        downloads.join(format!("{clean_artist} - {clean_title}.%(ext)s"))
    };

    let mut target_url = track_url.clone();
    if !is_safe_media_url(&target_url) || target_url.contains("deezer.com") {
        target_url = format!("ytsearch1:{clean_artist} {clean_title} official audio");
    }
    let mut clean_id = clean_track_id(&track_id);
    if clean_id.is_empty() {
        if let Some(m) = VIDEO_IN_URL.captures(&target_url).and_then(|c| c.get(1)) {
            clean_id = clean_track_id(m.as_str());
        }
    }

    if !clean_id.is_empty() {
        if let (Some(cached), Some(ffmpeg)) = (find_cached_file(&ctx, &clean_id).await, paths::resolve_binary(&ctx.paths, "ffmpeg")) {
            let mut args: Vec<String> = vec!["-y".into()];
            if let Some(s0) = start {
                args.extend(["-ss".into(), fmt_num(s0)]);
            }
            if let Some(e) = end {
                args.extend(["-to".into(), fmt_num(e)]);
            }
            args.extend(["-i".into(), cached.to_string_lossy().into_owned()]);
            if ["mp3", "m4a", "opus", "ogg"].contains(&ext.as_str()) {
                args.extend(["-b:a".into(), bitrate_for(&safe_quality).into()]);
            }
            args.extend(["-metadata".into(), format!("title={}", if title.is_empty() { &clean_title } else { &title })]);
            args.extend(["-metadata".into(), format!("artist={}", if artist.is_empty() { &clean_artist } else { &artist })]);
            args.push(out_path.to_string_lossy().into_owned());
            let out_for_guard = out_path.clone();
            let mut guard = AbortGuard::new(move || {
                let _ = std::fs::remove_file(&out_for_guard);
            });
            let result = ytdlp::run(&ffmpeg, &args, 600_000).await;
            guard.disarm();
            match result {
                Ok(()) => {
                    invalidate_downloads_index(&ctx);
                    return download_ok(raw, &temp_name, &out_filename, &downloads);
                }
                Err(_) => {
                    let _ = tokio::fs::remove_file(&out_path).await;
                }
            }
        }
    }

    let target_dir = if raw { tmp_dir.clone() } else { downloads.clone() };
    let before = list_names(&target_dir).await;
    let base_name = if raw { temp_name.trim_end_matches(".wav").to_string() } else { format!("{clean_artist} - {clean_title}") };
    let prefix = if raw { base_name.clone() } else { format!("{clean_artist} - {clean_title}.") };

    let mut guard = {
        let (dir, before, prefix) = (target_dir.clone(), before.clone(), prefix.clone());
        AbortGuard::new(move || {
            for f in list_names_sync(&dir) {
                if !before.contains(&f) && f.starts_with(&prefix) {
                    let _ = std::fs::remove_file(dir.join(&f));
                }
            }
        })
    };

    let can_carry_cover = !raw && ["mp3", "m4a", "flac", "opus", "ogg"].contains(&ext.as_str());
    let build_args = |with_extractor: bool| {
        let mut args = ytdlp::strs(&["--no-playlist", "--no-check-certificates", "--no-warnings", "-x", "--audio-format"]);
        args.push(ext.clone());
        args.push("--audio-quality".into());
        args.push(safe_quality.clone());
        args.push("--embed-metadata".into());
        args.push("-o".into());
        args.push(temp_template.to_string_lossy().into_owned());
        if with_extractor {
            args.extend(ytdlp::strs(&["--extractor-args", "youtube:player_client=android"]));
        }
        if can_carry_cover {
            args.extend(ytdlp::strs(&["--embed-thumbnail", "--convert-thumbnails", "jpg"]));
        }
        args.extend(trim_args.iter().cloned());
        args.push("--".into());
        args.push(target_url.clone());
        args
    };

    let label = if title.is_empty() { track_url.clone() } else { title.clone() };
    let primary = ytdlp::execute(&ctx.paths, build_args(true), DOWNLOAD_TIMEOUT_MS).await;
    if primary.is_ok() {
        guard.disarm();
        if !raw {
            cleanup_download_artifacts(&downloads, &before, &base_name, &clean_id, &ext, true).await;
            invalidate_downloads_index(&ctx);
        }
        return download_ok(raw, &temp_name, &out_filename, &downloads);
    }
    let primary_err = primary.unwrap_err();
    logger::error(&format!("Download primary failed: {label}"), ytdlp::truncate(&primary_err, 300));

    match ytdlp::execute(&ctx.paths, build_args(false), DOWNLOAD_TIMEOUT_MS).await {
        Ok(_) => {
            guard.disarm();
            if !raw {
                cleanup_download_artifacts(&downloads, &before, &base_name, &clean_id, &ext, true).await;
                invalidate_downloads_index(&ctx);
            }
            download_ok(raw, &temp_name, &out_filename, &downloads)
        }
        Err(e2) => {
            guard.disarm();
            if !raw {
                cleanup_download_artifacts(&downloads, &before, &base_name, &clean_id, &ext, false).await;
            } else {
                for f in list_names(&target_dir).await {
                    if !before.contains(&f) && f.starts_with(&prefix) {
                        let _ = tokio::fs::remove_file(target_dir.join(&f)).await;
                    }
                }
            }
            let msg = if e2.is_empty() { primary_err } else { e2 };
            logger::error(&format!("Download failure: {label}"), ytdlp::truncate(&msg, 300));
            json_response(500, &json!({ "success": false, "error": msg }))
        }
    }
}

pub async fn handle_tmp_file(_ctx: SharedCtx, req: Req) -> Response {
    let name = req.q("name");
    if !WAV_NAME.is_match(&name) || name.contains('/') || name.contains('\\') || name.contains("..") {
        return json_response(400, &json!({ "success": false, "error": "Invalid file name" }));
    }
    let path = downloads_dir().join(".tmp").join(&name);
    match tokio::fs::File::open(&path).await {
        Ok(f) => {
            let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
            Response::builder()
                .status(200)
                .header(header::CONTENT_TYPE, "audio/wav")
                .header(header::CONTENT_LENGTH, len)
                .body(Body::from_stream(tokio_util::io::ReaderStream::with_capacity(f, 64 * 1024)))
                .unwrap()
        }
        Err(_) => json_response(404, &json!({ "success": false, "error": "File not found" })),
    }
}

pub async fn handle_encode(ctx: SharedCtx, req: Req) -> Response {
    let source_name = req.q("name");
    let title = req.query.get("title").cloned().filter(|t| !t.is_empty()).unwrap_or_else(|| "untitled".into());
    let artist = req.query.get("artist").cloned().filter(|a| !a.is_empty()).unwrap_or_else(|| "unknown artist".into());
    let format = req.query.get("format").cloned().filter(|f| !f.is_empty()).unwrap_or_else(|| "mp3".into()).to_lowercase();
    let quality = req.query.get("quality").cloned().filter(|q| !q.is_empty()).unwrap_or_else(|| "0".into());
    let ext = if VALID_FORMATS.contains(&format.as_str()) { format } else { "mp3".into() };

    let downloads = downloads_dir();
    let tmp_dir = downloads.join(".tmp");
    let _ = tokio::fs::create_dir_all(&tmp_dir).await;
    let out_filename = format!("{} - {}.{ext}", sanitize_filename(&artist), sanitize_filename(&title));
    let out_path = downloads.join(&out_filename);
    let render_path = tmp_dir.join(format!("render_{}_{}.wav", now_ms(), random_base36(6)));

    let mut file = match tokio::fs::File::create(&render_path).await {
        Ok(f) => f,
        Err(e) => return json_response(400, &json!({ "success": false, "error": e.to_string() })),
    };
    let mut received = 0usize;
    let mut body = req.body.into_data_stream();
    while let Some(chunk) = body.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                drop(file);
                let _ = tokio::fs::remove_file(&render_path).await;
                return json_response(400, &json!({ "success": false, "error": e.to_string() }));
            }
        };
        received += chunk.len();
        if received > MAX_ENCODE_BODY {
            drop(file);
            let _ = tokio::fs::remove_file(&render_path).await;
            return json_response(413, &json!({ "success": false, "error": "WAV exceeds 300MB limit" }));
        }
        if let Err(e) = file.write_all(&chunk).await {
            drop(file);
            let _ = tokio::fs::remove_file(&render_path).await;
            return json_response(400, &json!({ "success": false, "error": e.to_string() }));
        }
    }
    let _ = file.flush().await;
    drop(file);

    let cleanup_sources = |render: PathBuf, tmp: PathBuf, src: String| async move {
        let _ = tokio::fs::remove_file(&render).await;
        if !src.is_empty() && WAV_NAME.is_match(&src) {
            let _ = tokio::fs::remove_file(tmp.join(&src)).await;
        }
    };

    let Some(ffmpeg) = paths::resolve_binary(&ctx.paths, "ffmpeg") else {
        let _ = tokio::fs::remove_file(&render_path).await;
        return json_response(500, &json!({ "success": false, "error": "ffmpeg not found" }));
    };
    let mut args: Vec<String> = vec!["-y".into(), "-i".into(), render_path.to_string_lossy().into_owned()];
    if ["mp3", "m4a", "opus", "ogg"].contains(&ext.as_str()) {
        args.extend(["-b:a".into(), bitrate_for(&quality).into()]);
    }
    args.extend(["-metadata".into(), format!("title={title}")]);
    args.extend(["-metadata".into(), format!("artist={artist}")]);
    args.push(out_path.to_string_lossy().into_owned());

    let mut guard = {
        let (r, o) = (render_path.clone(), out_path.clone());
        AbortGuard::new(move || {
            let _ = std::fs::remove_file(&r);
            let _ = std::fs::remove_file(&o);
        })
    };
    let result = ytdlp::run(&ffmpeg, &args, 600_000).await;
    guard.disarm();
    let response = match result {
        Ok(()) => {
            invalidate_downloads_index(&ctx);
            json_response(200, &json!({ "success": true, "filename": out_filename, "directory": downloads.to_string_lossy() }))
        }
        Err(e) => {
            logger::error(&format!("Encode failure: {title}"), ytdlp::truncate(&e, 300));
            let _ = tokio::fs::remove_file(&out_path).await;
            json_response(500, &json!({ "success": false, "error": if e.is_empty() { "Encoding failed".to_string() } else { e } }))
        }
    };
    cleanup_sources(render_path, tmp_dir, source_name).await;
    response
}
