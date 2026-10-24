use super::textutil::clean_track_id;
use super::{text_response, Req, SharedCtx};
use crate::paths;
use axum::body::Body;
use axum::http::{header, HeaderValue};
use axum::response::Response;
use futures_util::TryStreamExt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

pub const CACHE_EXTENSIONS: [&str; 4] = [".opus", ".webm", ".m4a", ".mp3"];
pub const AUDIO_EXTENSIONS: [&str; 10] =
    [".mp3", ".m4a", ".flac", ".opus", ".wav", ".ogg", ".oga", ".aac", ".webm", ".weba"];

pub fn library_mime(ext: &str) -> &'static str {
    match ext {
        ".mp3" => "audio/mpeg",
        ".wav" => "audio/wav",
        ".flac" => "audio/flac",
        ".opus" => "audio/ogg; codecs=opus",
        ".ogg" | ".oga" => "audio/ogg",
        ".m4a" => "audio/mp4",
        ".aac" => "audio/aac",
        ".webm" | ".weba" => "audio/webm",
        _ => "audio/mpeg",
    }
}

pub fn ext_of(p: &Path) -> String {
    p.extension().map(|e| format!(".{}", e.to_string_lossy().to_lowercase())).unwrap_or_default()
}

pub fn is_cache_audio(name: &str) -> bool {
    let lower = name.to_lowercase();
    CACHE_EXTENSIONS.iter().any(|e| lower.ends_with(e))
}

pub async fn find_cached_file(ctx: &SharedCtx, track_id: &str) -> Option<PathBuf> {
    let clean = clean_track_id(track_id);
    for ext in CACHE_EXTENSIONS {
        let p = ctx.paths.cache_dir.join(format!("{clean}{ext}"));
        if let Ok(meta) = tokio::fs::metadata(&p).await {
            if meta.len() > 60_000 {
                let touch = p.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    std::fs::File::options().write(true).open(&touch).and_then(|f| f.set_modified(SystemTime::now()))
                })
                .await;
                return Some(p);
            }
        }
    }
    None
}

pub async fn prune_disk_cache(ctx: &SharedCtx) {
    let Ok(mut entries) = tokio::fs::read_dir(&ctx.paths.cache_dir).await else { return };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(meta) = entry.metadata().await else { continue };
        if name.contains(".temp.") {
            let old = meta.modified().ok().and_then(|m| m.elapsed().ok()).map(|e| e > Duration::from_secs(600)).unwrap_or(false);
            if old {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
            continue;
        }
        if meta.len() < 1000 {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

fn parse_range(range: &str, size: u64) -> Option<(u64, u64)> {
    let spec = range.trim().strip_prefix("bytes=")?.split(',').next()?.trim();
    let (a, b) = spec.split_once('-')?;
    if size == 0 {
        return None;
    }
    let (start, end) = if a.trim().is_empty() {
        let n: u64 = b.trim().parse().ok()?;
        (size.saturating_sub(n), size - 1)
    } else {
        let start: u64 = a.trim().parse().ok()?;
        let end = if b.trim().is_empty() { size - 1 } else { b.trim().parse::<u64>().ok()?.min(size - 1) };
        (start, end)
    };
    (start <= end && start < size).then_some((start, end))
}

pub async fn serve_file(path: &Path, content_type: &str, range: Option<String>) -> std::io::Result<Response> {
    let mut file = tokio::fs::File::open(path).await?;
    let size = file.metadata().await?.len();
    let ct = HeaderValue::from_str(content_type).unwrap_or(HeaderValue::from_static("application/octet-stream"));
    if let Some(r) = range {
        let Some((start, end)) = parse_range(&r, size) else {
            return Ok(Response::builder()
                .status(416)
                .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                .body(Body::empty())
                .unwrap());
        };
        file.seek(std::io::SeekFrom::Start(start)).await?;
        let len = end - start + 1;
        let stream = ReaderStream::with_capacity(file.take(len), 64 * 1024);
        return Ok(Response::builder()
            .status(206)
            .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"))
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, len)
            .header(header::CONTENT_TYPE, ct)
            .body(Body::from_stream(stream))
            .unwrap());
    }
    let stream = ReaderStream::with_capacity(file, 64 * 1024);
    Ok(Response::builder()
        .status(200)
        .header(header::CONTENT_LENGTH, size)
        .header(header::CONTENT_TYPE, ct)
        .header(header::ACCEPT_RANGES, "bytes")
        .body(Body::from_stream(stream))
        .unwrap())
}

pub async fn handle_local_track(ctx: SharedCtx, req: Req) -> Response {
    let clean = clean_track_id(&req.q("id"));
    let Some(path) = find_cached_file(&ctx, &clean).await else {
        return text_response(404, "File not found in local cache");
    };
    let ct = match ext_of(&path).as_str() {
        ".opus" => "audio/ogg; codecs=opus",
        ".webm" => "audio/webm",
        ".m4a" => "audio/mp4",
        _ => "audio/mpeg",
    };
    serve_file(&path, ct, req.header("range")).await.unwrap_or_else(|_| text_response(500, "Error reading local file"))
}

pub async fn handle_library_file(ctx: SharedCtx, req: Req) -> Response {
    let id = req.q("id");
    let file = ctx.library.lock().get(&id).and_then(|v| v.get("file")).and_then(|f| f.as_str()).map(str::to_string);
    let Some(file) = file.filter(|f| !f.is_empty()) else {
        return text_response(404, "File not found in library");
    };
    let path = ctx.paths.library_dir.join(&file);
    let ct = library_mime(&ext_of(&path));
    serve_file(&path, ct, req.header("range")).await.unwrap_or_else(|_| text_response(404, "File not found in library"))
}

pub fn downloads_dir() -> PathBuf {
    let dir = paths::resolve_download_dir().unwrap_or_else(paths::default_downloads_dir);
    if !dir.exists() {
        let _ = std::fs::create_dir_all(&dir);
    }
    dir
}

pub async fn handle_downloaded_track(_ctx: SharedCtx, req: Req) -> Response {
    let raw = req.q("name");
    let safe = Path::new(&raw).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let dir = downloads_dir();
    let path = dir.join(&safe);
    let ext = ext_of(Path::new(&safe));
    if safe.is_empty() || !AUDIO_EXTENSIONS.contains(&ext.as_str()) || path.parent() != Some(dir.as_path()) {
        return text_response(404, "File not found in downloads");
    }
    serve_file(&path, library_mime(&ext), req.header("range"))
        .await
        .unwrap_or_else(|_| text_response(404, "File not found in downloads"))
}

pub async fn handle_local_thumbnail(ctx: SharedCtx, req: Req) -> Response {
    let clean = clean_track_id(&req.q("id"));
    let path = ctx.paths.thumbnails_dir.join(format!("{clean}.jpg"));
    match tokio::fs::File::open(&path).await {
        Ok(f) => Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "image/jpeg")
            .header(header::CACHE_CONTROL, "public, max-age=31536000")
            .body(Body::from_stream(ReaderStream::new(f)))
            .unwrap(),
        Err(_) => text_response(404, "Thumbnail not cached"),
    }
}

fn ipv4_is_private(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 0
        || a == 10
        || a == 127
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 100 && (64..=127).contains(&b))
        || a >= 224
}

fn ipv6_is_private(ip: Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() {
        return true;
    }
    let seg0 = ip.segments()[0];
    if seg0 & 0xffc0 == 0xfe80 || seg0 & 0xfe00 == 0xfc00 {
        return true;
    }
    if let Some(v4) = ip.to_ipv4_mapped() {
        return ipv4_is_private(v4);
    }
    false
}

fn ip_is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_private(v4),
        IpAddr::V6(v6) => ipv6_is_private(v6),
    }
}

fn normalize_numeric_host(h: &str) -> Option<Ipv4Addr> {
    let n = if h.len() < 11 && !h.is_empty() && h.chars().all(|c| c.is_ascii_digit()) {
        h.parse::<u64>().ok()
    } else if let Some(hex) = h.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).ok()
    } else {
        None
    }?;
    (n <= 0xffff_ffff).then(|| Ipv4Addr::from(n as u32))
}

pub async fn is_private_host(hostname: &str) -> bool {
    let lowered = hostname.to_lowercase();
    let host: String = lowered.trim_start_matches('[').trim_end_matches(']').to_string();
    let host = host.as_str();
    if host.is_empty() || host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") || host.ends_with(".internal") {
        return true;
    }
    if let Some(v4) = normalize_numeric_host(host) {
        return ipv4_is_private(v4);
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return ip_is_private(ip);
    }
    let target = format!("{host}:443");
    match tokio::net::lookup_host(target).await {
        Ok(addrs) => {
            let list: Vec<_> = addrs.collect();
            list.is_empty() || list.iter().any(|a| ip_is_private(a.ip()))
        }
        Err(_) => true,
    }
}

const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

pub async fn handle_audio_proxy(ctx: SharedCtx, req: Req) -> Response {
    let target = req.q("url");
    if target.is_empty() {
        return text_response(400, "Missing url");
    }
    let Ok(parsed) = url::Url::parse(&target) else {
        return text_response(400, "Invalid url");
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return text_response(400, "Unsupported protocol");
    }
    if is_private_host(parsed.host_str().unwrap_or("")).await {
        return text_response(403, "Blocked target");
    }
    let method = if req.method == axum::http::Method::HEAD { reqwest::Method::HEAD } else { reqwest::Method::GET };
    let mut upstream = ctx.proxy_http.request(method, parsed);
    for key in ["range", "accept", "accept-language", "user-agent"] {
        if let Some(v) = req.headers.get(key) {
            upstream = upstream.header(key, v.as_bytes());
        }
    }
    let resp = match upstream.send().await {
        Ok(r) => r,
        Err(_) => return text_response(502, "Proxy error"),
    };
    if is_hls_playlist(&resp) {
        return proxy_hls_playlist(&ctx, resp).await;
    }
    let mut builder = Response::builder().status(resp.status().as_u16());
    for (k, v) in resp.headers() {
        if !HOP_BY_HOP.contains(&k.as_str()) {
            builder = builder.header(k.as_str(), v.as_bytes());
        }
    }
    let stream = resp.bytes_stream().map_err(std::io::Error::other);
    builder.body(Body::from_stream(stream)).unwrap_or_else(|_| text_response(500, "Proxy failed"))
}

fn is_hls_playlist(resp: &reqwest::Response) -> bool {
    let ct = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    ct.contains("mpegurl") || resp.url().path().to_ascii_lowercase().ends_with(".m3u8")
}

pub fn rewrite_hls(text: &str, base: &url::Url, proxy_base: &str) -> String {
    let proxied = |uri: &str| -> String {
        match base.join(uri.trim()) {
            Ok(abs) => format!("{proxy_base}/api/audio-proxy?url={}", urlencoding::encode(abs.as_str())),
            Err(_) => uri.to_string(),
        }
    };
    let mut out = String::with_capacity(text.len() * 2);
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            out.push_str(line);
        } else if trimmed.starts_with('#') {
            match trimmed.find("URI=\"") {
                Some(start) => {
                    let value_start = start + 5;
                    match trimmed[value_start..].find('"') {
                        Some(len) => {
                            let uri = &trimmed[value_start..value_start + len];
                            out.push_str(&trimmed[..value_start]);
                            out.push_str(&proxied(uri));
                            out.push_str(&trimmed[value_start + len..]);
                        }
                        None => out.push_str(line),
                    }
                }
                None => out.push_str(line),
            }
        } else {
            out.push_str(&proxied(trimmed));
        }
        out.push('\n');
    }
    out
}

async fn proxy_hls_playlist(ctx: &SharedCtx, resp: reqwest::Response) -> Response {
    let status = resp.status().as_u16();
    let base = resp.url().clone();
    let text = match resp.text().await {
        Ok(t) => t,
        Err(_) => return text_response(502, "Proxy error"),
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(rewrite_hls(&text, &base, &ctx.base())))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hls_rewrite() {
        let base = url::Url::parse("https://cdn.example.com/a/b/playlist.m3u8?sig=1").unwrap();
        let src = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:10,\nseg1.ts?x=1\n\nhttps://other.example.com/seg2.ts\n";
        let out = rewrite_hls(src, &base, "http://127.0.0.1:1");
        assert!(out.contains("URI=\"http://127.0.0.1:1/api/audio-proxy?url=https%3A%2F%2Fcdn.example.com%2Fa%2Fb%2Fkey.bin\""));
        assert!(out.contains("\nhttp://127.0.0.1:1/api/audio-proxy?url=https%3A%2F%2Fcdn.example.com%2Fa%2Fb%2Fseg1.ts%3Fx%3D1\n"));
        assert!(out.contains("\nhttp://127.0.0.1:1/api/audio-proxy?url=https%3A%2F%2Fother.example.com%2Fseg2.ts\n"));
        assert!(out.starts_with("#EXTM3U\n#EXT-X-KEY"));
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-", 100), Some((0, 99)));
        assert_eq!(parse_range("bytes=10-19", 100), Some((10, 19)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=200-", 100), None);
    }

    #[test]
    fn private_ips() {
        assert!(ip_is_private("10.1.2.3".parse().unwrap()));
        assert!(ip_is_private("::ffff:192.168.1.1".parse().unwrap()));
        assert!(ip_is_private("fe80::1".parse().unwrap()));
        assert!(!ip_is_private("142.250.1.1".parse().unwrap()));
        assert_eq!(normalize_numeric_host("2130706433"), Some(Ipv4Addr::new(127, 0, 0, 1)));
    }
}
