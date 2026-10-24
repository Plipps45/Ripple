use super::download::find_downloaded_file;
use super::lyrics::sanitize_for_lyrics;
use super::media::find_cached_file;
use super::textutil::*;
use super::{json_response, ytdlp, Req, SharedCtx};
use crate::logger;
use axum::response::Response;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

const STREAM_CACHE_TTL: Duration = Duration::from_secs(1800);

fn id_from_url(url: &str) -> String {
    url.split_once("v=").map(|(_, rest)| clean_track_id(rest.split('&').next().unwrap_or(""))).unwrap_or_default()
}

pub(crate) fn resolve_args(platform: &str, target: &str) -> Vec<String> {
    let mut args = ytdlp::strs(&[
        "--no-config", "-g", "--no-warnings", "--no-playlist", "--no-check-certificates", "--prefer-free-formats", "-f", "bestaudio/best",
    ]);
    if platform == "youtube" {
        args.extend(ytdlp::strs(&["--extractor-args", "youtube:player_client=android"]));
    }
    args.push("--".into());
    args.push(target.to_string());
    args
}

fn youtube_search_resolve_args(query: &str) -> Vec<String> {
    let mut args = ytdlp::strs(&["--no-config", "-g", "--no-warnings", "--no-check-certificates", "--prefer-free-formats", "-f", "bestaudio/best"]);
    args.extend(ytdlp::strs(&["--extractor-args", "youtube:player_client=android", "--"]));
    args.push(format!("ytsearch1:{query}"));
    args
}

static VARIANT_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(remix|rmx|edit|bootleg|flip|cover|instrumental|karaoke|nightcore|sped\s*up|slowed|reverb|8d|acapella|live|mashup|remake|type\s*beat)\b",
    )
    .unwrap()
});

fn fallback_query(title: &str, artist: &str) -> String {
    let title = match title.split_once(" - ") {
        Some((head, rest)) if !artist.is_empty() && head.trim().eq_ignore_ascii_case(artist.trim()) => rest,
        _ => title,
    };
    let (t, a) = sanitize_for_lyrics(title, artist);
    format!("{a} {t}").trim().to_string()
}

fn rank_soundcloud_match(meta: &TrackMeta, title: &str, duration: f64) -> i32 {
    let mut score = 0;
    if VARIANT_TITLE.is_match(title) && !VARIANT_TITLE.is_match(&meta.title) {
        score -= 4;
    }
    if meta.duration > 0.0 && !is_video_title(&meta.title) {
        let off = (duration - meta.duration).abs();
        score += if off <= 4.0 { 3 } else if off <= 15.0 { 1 } else if off <= 40.0 { -1 } else { -5 };
    }
    score
}

async fn soundcloud_fallback(ctx: &SharedCtx, query: &str, meta: &TrackMeta) -> Result<Option<String>, String> {
    let mut args = ytdlp::strs(&[
        "--no-config", "--no-warnings", "--no-check-certificates", "--prefer-free-formats", "-f", "bestaudio/best",
        "-i", "--playlist-items", "1:8", "--print", "%(duration)s\t%(title)s\t%(url)s", "--",
    ]);
    args.push(format!("scsearch8:{query}"));
    let raw = ytdlp::execute_partial(&ctx.paths, args, 30_000).await?;
    let best = raw
        .lines()
        .filter_map(|line| {
            let mut parts = line.trim().splitn(3, '\t');
            let duration = parts.next()?.parse::<f64>().unwrap_or(0.0);
            let (title, url) = (parts.next()?, parts.next()?);
            url.starts_with("http").then(|| (rank_soundcloud_match(meta, title, duration), url.to_string()))
        })
        .enumerate()
        .max_by_key(|(i, (rank, _))| (*rank, -(*i as i64)))
        .map(|(_, (_, url))| url);
    Ok(best)
}

fn is_permanent_failure(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    ["drm", "unavailable", "private video", "removed", "not available", "copyright", "sign in to confirm your age", "members-only"]
        .iter()
        .any(|k| e.contains(k))
}

fn stream_payload(ctx: &SharedCtx, stream_url: &str) -> Value {
    let proxied = format!("{}/api/audio-proxy?url={}", ctx.base(), enc(stream_url));
    json!({
        "success": true,
        "isLocal": false,
        "streamUrl": proxied,
        "isHls": stream_url.contains(".m3u8"),
        "proxyUrl": proxied,
        "directUrl": stream_url
    })
}

fn cached_stream(ctx: &SharedCtx, keys: &[&str]) -> Option<Value> {
    let c = ctx.caches.lock();
    keys.iter()
        .filter(|k| !k.is_empty())
        .find_map(|k| c.stream_url.get(*k))
        .filter(|(at, _)| at.elapsed() < STREAM_CACHE_TTL)
        .map(|(_, v)| v.clone())
}

fn remember_stream(ctx: &SharedCtx, cache_key: &str, clean_id: &str, data: &Value) {
    let mut c = ctx.caches.lock();
    if !cache_key.is_empty() {
        c.stream_url.insert(cache_key.to_string(), (Instant::now(), data.clone()));
    }
    if !clean_id.is_empty() && clean_id != cache_key {
        c.stream_url.insert(clean_id.to_string(), (Instant::now(), data.clone()));
    }
}

#[derive(Clone, Default)]
pub struct TrackMeta {
    pub title: String,
    pub artist: String,
    pub duration: f64,
    pub thumbnail: String,
}

fn schedule_disk_download(ctx: &SharedCtx, url: String, clean_id: String, platform: String, meta: TrackMeta) {
    if clean_id.is_empty() {
        return;
    }
    let c = ctx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(10)).await;
        download_track_to_disk(c, url, clean_id, platform, meta).await;
    });
}

pub async fn handle_stream_info(ctx: SharedCtx, req: Req) -> Response {
    let mut track_url = req.q("url");
    let platform = req.query.get("platform").cloned().filter(|p| !p.is_empty()).unwrap_or_else(|| "youtube".into());
    let meta = TrackMeta {
        title: req.q("title"),
        artist: req.q("artist"),
        duration: req.q("duration").parse().unwrap_or(0.0),
        thumbnail: req.q("thumbnail"),
    };
    let mut clean_id = clean_track_id(&req.q("id"));
    if clean_id.is_empty() && track_url.contains("v=") {
        clean_id = id_from_url(&track_url);
    }

    let (cached, downloaded) = tokio::join!(
        async { if clean_id.is_empty() { None } else { find_cached_file(&ctx, &clean_id).await } },
        async {
            if meta.title.is_empty() && meta.artist.is_empty() {
                None
            } else {
                find_downloaded_file(&ctx, &meta.title, &meta.artist).await
            }
        }
    );
    if cached.is_some() {
        return json_response(200, &json!({
            "success": true, "isLocal": true,
            "streamUrl": format!("{}/api/local-track?id={}", ctx.base(), enc(&clean_id))
        }));
    }
    if let Some(name) = downloaded {
        return json_response(200, &json!({
            "success": true, "isLocal": true, "isDownload": true,
            "streamUrl": format!("{}/api/downloaded-track?name={}", ctx.base(), enc(&name))
        }));
    }

    let cache_key = if !clean_id.is_empty() {
        clean_id.clone()
    } else if !track_url.is_empty() {
        track_url.clone()
    } else {
        format!("{}_{}", meta.artist, meta.title)
    }
    .to_lowercase();
    if let Some(hit) = cached_stream(&ctx, &[&cache_key, &clean_id]) {
        schedule_disk_download(&ctx, track_url.clone(), clean_id.clone(), platform.clone(), meta);
        return json_response(200, &hit);
    }

    if platform == "youtube" && !track_url.starts_with("http") && !clean_id.is_empty() {
        track_url = format!("https://www.youtube.com/watch?v={clean_id}");
    }

    let label = if meta.title.is_empty() { track_url.clone() } else { meta.title.clone() };
    let mut stream_url = None;
    if is_safe_media_url(&track_url) {
        for attempt in 0..2 {
            match ytdlp::execute(&ctx.paths, resolve_args(&platform, &track_url), 18_000).await {
                Ok(raw) => {
                    stream_url = ytdlp::first_http_line(&raw);
                    break;
                }
                Err(e) => {
                    logger::error(&format!("Stream resolve primary error: {label}"), &e);
                    if attempt == 1 || is_permanent_failure(&e) {
                        break;
                    }
                }
            }
        }
    }
    let from_primary = stream_url.is_some();
    if stream_url.is_none() && (!meta.title.is_empty() || !meta.artist.is_empty() || !track_url.is_empty()) {
        let q = fallback_query(&meta.title, &meta.artist);
        let q = if q.is_empty() { track_url.clone() } else { q };
        let order = if platform == "soundcloud" { ["youtube", "soundcloud"] } else { ["soundcloud", "youtube"] };
        for source in order {
            let found = if source == "soundcloud" {
                soundcloud_fallback(&ctx, &q, &meta).await
            } else {
                ytdlp::execute_partial(&ctx.paths, youtube_search_resolve_args(&q), 25_000).await.map(|raw| ytdlp::first_http_line(&raw))
            };
            match found {
                Ok(url) => stream_url = url,
                Err(e) => logger::error(&format!("Stream resolve fallback error ({source}): {label}"), e),
            }
            if stream_url.is_some() {
                break;
            }
        }
    }
    let Some(stream_url) = stream_url else {
        logger::log("ERROR", &format!("Stream resolve failed: {label}"));
        return json_response(500, &json!({ "success": false, "error": "could not extract audio stream" }));
    };

    let data = stream_payload(&ctx, &stream_url);
    remember_stream(&ctx, &cache_key, &clean_id, &data);
    if from_primary {
        schedule_disk_download(&ctx, track_url, clean_id, platform, meta);
    }
    json_response(200, &data)
}

pub async fn prefetch_track(ctx: SharedCtx, track_url: String, track_id: String, platform: String) {
    let mut clean_id = clean_track_id(&track_id);
    if clean_id.is_empty() && track_url.contains("v=") {
        clean_id = id_from_url(&track_url);
    }
    let cache_key = if clean_id.is_empty() { track_url.clone() } else { clean_id.clone() }.to_lowercase();
    if cache_key.is_empty() || ctx.prefetching.lock().len() > 2 {
        return;
    }
    let already = (!clean_id.is_empty() && find_cached_file(&ctx, &clean_id).await.is_some()) || {
        let c = ctx.caches.lock();
        c.stream_url.contains_key(&cache_key) || (!clean_id.is_empty() && c.stream_url.contains_key(&clean_id))
    };
    if already || !ctx.prefetching.lock().insert(cache_key.clone()) {
        return;
    }
    let mut target = track_url;
    if platform == "youtube" && !target.starts_with("http") && !clean_id.is_empty() {
        target = format!("https://www.youtube.com/watch?v={clean_id}");
    }
    if is_safe_media_url(&target) {
        if let Ok(raw) = ytdlp::execute(&ctx.paths, resolve_args(&platform, &target), 18_000).await {
            if let Some(url) = ytdlp::first_http_line(&raw) {
                let data = stream_payload(&ctx, &url);
                remember_stream(&ctx, &cache_key, &clean_id, &data);
            }
        }
    }
    ctx.prefetching.lock().remove(&cache_key);
}

pub async fn cache_thumbnail_to_disk(ctx: &SharedCtx, clean_id: &str, url: &str) {
    if clean_id.is_empty() || !url.starts_with("http") {
        return;
    }
    let dest = ctx.paths.thumbnails_dir.join(format!("{clean_id}.jpg"));
    if dest.exists() {
        return;
    }
    if let Ok(resp) = ctx.http.get(url).timeout(Duration::from_secs(15)).send().await {
        if resp.status().is_success() {
            if let Ok(bytes) = resp.bytes().await {
                if bytes.len() > 500 {
                    let _ = tokio::fs::write(&dest, &bytes).await;
                }
            }
        }
    }
}

pub async fn download_track_to_disk(ctx: SharedCtx, mut track_url: String, track_id: String, platform: String, meta: TrackMeta) {
    let clean_id = clean_track_id(&track_id);
    if ctx.downloading.lock().contains(&clean_id) || find_cached_file(&ctx, &clean_id).await.is_some() {
        return;
    }
    if !ctx.downloading.lock().insert(clean_id.clone()) {
        return;
    }
    if platform == "youtube" && !track_url.starts_with("http") && super::search::is_video_id(&clean_id) {
        track_url = format!("https://www.youtube.com/watch?v={clean_id}");
    }
    let output = ctx.paths.cache_dir.join(format!("{clean_id}.opus"));
    let temp = ctx.paths.cache_dir.join(format!("{clean_id}.temp.opus"));
    let mut args = ytdlp::strs(&["--no-config", "--no-playlist", "--no-check-certificates", "--no-warnings", "-x", "--audio-format", "opus", "--audio-quality", "128K", "-o"]);
    args.push(temp.to_string_lossy().into_owned());
    if platform == "youtube" {
        args.extend(ytdlp::strs(&["--extractor-args", "youtube:player_client=android"]));
    }
    args.push(track_url.clone());

    if !meta.thumbnail.is_empty() {
        let (c, id, thumb) = (ctx.clone(), clean_id.clone(), meta.thumbnail.clone());
        tokio::spawn(async move { cache_thumbnail_to_disk(&c, &id, &thumb).await });
    }

    let label = if meta.title.is_empty() { track_id.clone() } else { meta.title.clone() };
    let result = ytdlp::execute(&ctx.paths, args, 0).await;
    ctx.downloading.lock().remove(&clean_id);
    match result {
        Ok(_) if temp.exists() => {
            let size = tokio::fs::metadata(&temp).await.map(|m| m.len()).unwrap_or(0);
            if size > 60_000 && tokio::fs::rename(&temp, &output).await.is_ok() {
                let mut m = Map::new();
                m.insert("id".into(), json!(track_id));
                m.insert("title".into(), json!(meta.title));
                m.insert("artist".into(), json!(meta.artist));
                m.insert("duration".into(), jnum(meta.duration));
                m.insert("thumbnail".into(), json!(meta.thumbnail));
                m.insert("platform".into(), json!(if platform.is_empty() { "youtube".to_string() } else { platform }));
                m.insert("fileSize".into(), json!(size));
                ctx.save_track_metadata(&clean_id, m);
            } else {
                let _ = tokio::fs::remove_file(&temp).await;
            }
        }
        Ok(_) => {}
        Err(e) => {
            logger::log("ERROR", &format!("Download failure: {label} ({})", ytdlp::truncate(&e, 200)));
            let _ = tokio::fs::remove_file(&temp).await;
        }
    }
}

pub async fn handle_check_cache(ctx: SharedCtx, req: Req) -> Response {
    let clean = clean_track_id(&req.q("id"));
    let cached = find_cached_file(&ctx, &clean).await.is_some();
    json_response(200, &json!({
        "cached": cached,
        "streamUrl": if cached { json!(format!("{}/api/local-track?id={}", ctx.base(), enc(&clean))) } else { Value::Null }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(title: &str, duration: f64) -> TrackMeta {
        TrackMeta { title: title.into(), artist: "The Weeknd".into(), duration, thumbnail: String::new() }
    }

    #[test]
    fn fallback_query_is_the_plain_song() {
        assert_eq!(fallback_query("The Weeknd - Blinding Lights (Official Video)", "The Weeknd"), "The Weeknd Blinding Lights");
        assert_eq!(fallback_query("Blinding Lights", "The Weeknd"), "The Weeknd Blinding Lights");
    }

    #[test]
    fn the_song_beats_its_remixes() {
        let video = meta("The Weeknd - Blinding Lights (Official Video)", 262.0);
        assert!(rank_soundcloud_match(&video, "Blinding Lights", 200.0) > rank_soundcloud_match(&video, "Blinding Lights (Lewis Dk Remix)", 206.0));
        let song = meta("Blinding Lights", 200.0);
        assert!(rank_soundcloud_match(&song, "Blinding Lights", 201.0) > rank_soundcloud_match(&song, "Blinding Lights full", 762.0));
        assert!(rank_soundcloud_match(&song, "Blinding Lights (Faze2 Edit)", 192.0) > rank_soundcloud_match(&song, "Blinding Lights full", 762.0));
        let remix = meta("Blinding Lights (Chromatics Remix)", 300.0);
        assert!(rank_soundcloud_match(&remix, "Blinding Lights (Chromatics Remix)", 300.0) > 0);
    }
}
