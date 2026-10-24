pub mod artist_bio;
pub mod download;
pub mod imports;
pub mod library;
pub mod lyrics;
pub mod lyrics_sync;
pub mod media;
pub mod recommend;
pub mod search;
pub mod stream;
pub mod textutil;
pub mod tools;
pub mod ytdlp;

use crate::paths::Paths;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::Router;
use parking_lot::Mutex;
use rand::RngCore;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

pub const DEFAULT_PORT: u16 = 38472;
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

const TOKEN_PROTECTED_ROUTES: &[&str] = &[
    "/api/state",
    "/api/translate",
    "/api/vibe",
    "/api/search",
    "/api/artist",
    "/api/lyrics",
    "/api/lyrics-offset",
    "/api/download",
    "/api/spotify-import",
    "/api/spotify-auth-start",
    "/api/spotify-status",
    "/api/spotify-library",
    "/api/spotify-logout",
    "/api/prefetch",
    "/api/check-cache",
    "/api/stream-info",
    "/api/track-analysis",
    "/api/tools-status",
    "/api/tools-install",
    "/api/library",
    "/api/library-add",
    "/api/library-update",
    "/api/library-remove",
    "/api/offline-library",
    "/api/offline-delete",
    "/api/clear-offline-library",
    "/api/save-custom-lyrics",
    "/api/encode",
];

pub struct Timed<T> {
    pub expires: Instant,
    pub value: T,
}

#[derive(Default)]
pub struct RecentSet {
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl RecentSet {
    pub fn contains(&self, k: &str) -> bool {
        self.set.contains(k)
    }
    pub fn add(&mut self, k: String, cap: usize) {
        if self.set.insert(k.clone()) {
            self.order.push_back(k);
        }
        while self.order.len() > cap {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
    }
}

#[derive(Default)]
pub struct SpotifyAuth {
    pub verifier: String,
    pub state: String,
    pub client_id: String,
    pub token: String,
    pub error: String,
}

#[derive(Default)]
pub struct Caches {
    pub search: HashMap<String, Timed<Value>>,
    pub stream_url: HashMap<String, (Instant, Value)>,
    pub lyrics: HashMap<String, (String, bool)>,
    pub artist: HashMap<String, Value>,
    pub vibe: HashMap<String, Timed<Vec<Value>>>,
    pub yt_artist: HashMap<String, String>,
    pub dz_artist_id: HashMap<String, Timed<Option<String>>>,
    pub yt_mix: HashMap<String, Timed<Vec<Value>>>,
    pub resolved: HashMap<String, Timed<Option<Value>>>,
    pub resolved_order: VecDeque<String>,
    pub album: HashMap<String, Timed<Value>>,
    pub album_order: VecDeque<String>,
    pub hot_picks: Option<Timed<Value>>,
    pub recent_ids: RecentSet,
    pub recent_keys: RecentSet,
    pub ytm_fail_until: Option<Instant>,
    pub ytm_fail_count: u32,
    pub downloads_index: Option<(Instant, HashMap<String, String>)>,
}

#[derive(Clone, Default, serde::Serialize)]
pub struct ToolsProgress {
    pub status: String,
    pub tool: String,
    pub percent: u32,
    pub error: String,
}

pub struct Ctx {
    pub paths: Paths,
    pub token: String,
    pub port: AtomicU16,
    pub http: reqwest::Client,
    pub proxy_http: reqwest::Client,
    pub metadata: Mutex<Map<String, Value>>,
    pub library: Mutex<Map<String, Value>>,
    pub caches: Mutex<Caches>,
    pub downloading: Mutex<HashSet<String>>,
    pub prefetching: Mutex<HashSet<String>>,
    pub app_state: Mutex<Value>,
    pub sse: broadcast::Sender<String>,
    pub spotify: Mutex<SpotifyAuth>,
    pub tools: Mutex<ToolsProgress>,
}

pub type SharedCtx = Arc<Ctx>;

impl Ctx {
    pub fn new(paths: Paths) -> SharedCtx {
        paths.ensure_dirs();
        let read_map = |p: &std::path::Path| -> Map<String, Value> {
            std::fs::read_to_string(p)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default()
        };
        let mut token_bytes = [0u8; 24];
        rand::thread_rng().fill_bytes(&mut token_bytes);
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .expect("http client");
        let proxy_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
            .build()
            .expect("proxy client");
        let (sse, _) = broadcast::channel(64);
        Arc::new(Ctx {
            metadata: Mutex::new(read_map(&paths.metadata_path)),
            library: Mutex::new(read_map(&paths.library_path)),
            paths,
            token: hex::encode(token_bytes),
            port: AtomicU16::new(DEFAULT_PORT),
            http,
            proxy_http,
            caches: Mutex::new(Caches::default()),
            downloading: Mutex::new(HashSet::new()),
            prefetching: Mutex::new(HashSet::new()),
            app_state: Mutex::new(json!({})),
            sse,
            spotify: Mutex::new(SpotifyAuth::default()),
            tools: Mutex::new(ToolsProgress { status: "idle".into(), ..Default::default() }),
        })
    }

    pub fn port(&self) -> u16 {
        self.port.load(Ordering::Relaxed)
    }

    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port())
    }

    pub fn save_metadata(&self) {
        let snapshot = Value::Object(self.metadata.lock().clone());
        let _ = std::fs::write(&self.paths.metadata_path, serde_json::to_string_pretty(&snapshot).unwrap_or_default());
    }

    pub fn save_track_metadata(&self, clean_id: &str, meta: Map<String, Value>) {
        if clean_id.is_empty() {
            return;
        }
        {
            let mut idx = self.metadata.lock();
            let entry = idx.entry(clean_id.to_string()).or_insert_with(|| json!({}));
            if let Value::Object(m) = entry {
                m.extend(meta);
                m.insert("lastPlayed".into(), json!(textutil::now_ms()));
            }
        }
        self.save_metadata();
    }

    pub fn save_library(&self) {
        let snapshot = Value::Object(self.library.lock().clone());
        let _ = std::fs::write(&self.paths.library_path, serde_json::to_string_pretty(&snapshot).unwrap_or_default());
    }

    pub fn token_matches(&self, provided: Option<&str>) -> bool {
        let Some(given) = provided else { return false };
        let (a, b) = (given.as_bytes(), self.token.as_bytes());
        if a.len() != b.len() {
            return false;
        }
        a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

pub fn json_response(status: u16, body: &Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

pub fn text_response(status: u16, body: &str) -> Response {
    Response::builder().status(status).body(Body::from(body.to_string())).unwrap()
}

pub fn html_response(status: u16, body: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(body.to_string()))
        .unwrap()
}

pub fn empty_response(status: u16) -> Response {
    Response::builder().status(status).body(Body::empty()).unwrap()
}

pub fn payload_too_large() -> Response {
    json_response(413, &json!({ "success": false, "error": "payload too large" }))
}

pub struct Req {
    pub method: Method,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HeaderMap,
    pub body: Body,
}

impl Req {
    pub fn q(&self, key: &str) -> String {
        self.query.get(key).cloned().unwrap_or_default()
    }

    pub fn has_q(&self, key: &str) -> bool {
        self.query.contains_key(key)
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
    }

    pub async fn json_body(self) -> Result<String, Response> {
        match axum::body::to_bytes(self.body, MAX_BODY_BYTES).await {
            Ok(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
            Err(_) => Err(payload_too_large()),
        }
    }
}

fn parse_query(raw: Option<&str>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(q) = raw {
        for (k, v) in url::form_urlencoded::parse(q.as_bytes()) {
            out.entry(k.into_owned()).or_insert_with(|| v.into_owned());
        }
    }
    out
}

fn origin_allowed(origin: &str) -> bool {
    if origin == "null" {
        return true;
    }
    match url::Url::parse(origin) {
        Ok(u) => {
            let host = u.host_str().unwrap_or("").trim_matches(|c| c == '[' || c == ']');
            u.scheme() == "file"
                || u.scheme() == "tauri"
                || matches!(host, "127.0.0.1" | "localhost" | "::1" | "tauri.localhost")
        }
        Err(_) => false,
    }
}

fn host_allowed(host: &str) -> bool {
    let name = if host.starts_with('[') {
        host.split(']').next().unwrap_or("").trim_start_matches('[')
    } else {
        host.rsplit_once(':').map(|(h, p)| if p.chars().all(|c| c.is_ascii_digit()) { h } else { host }).unwrap_or(host)
    };
    name.is_empty() || matches!(name, "127.0.0.1" | "localhost" | "::1")
}

fn apply_cors(mut res: Response) -> Response {
    let h = res.headers_mut();
    for (k, v) in [
        ("access-control-allow-origin", "*"),
        ("access-control-allow-headers", "*"),
        ("access-control-allow-methods", "GET, POST, OPTIONS"),
    ] {
        if !h.contains_key(k) {
            h.insert(k, HeaderValue::from_static(v));
        }
    }
    res
}

async fn entry(State(ctx): State<SharedCtx>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let forbidden = || json_response(403, &json!({ "success": false, "error": "forbidden" }));

    if let Some(origin) = parts.headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if !origin_allowed(origin) {
            return apply_cors(forbidden());
        }
    }
    if let Some(host) = parts.headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        if !host_allowed(host) {
            return apply_cors(forbidden());
        }
    }
    if parts.method == Method::OPTIONS {
        return apply_cors(empty_response(204));
    }

    let path = parts.uri.path().to_string();
    if TOKEN_PROTECTED_ROUTES.contains(&path.as_str()) {
        let given = parts.headers.get("x-ripple-token").and_then(|v| v.to_str().ok());
        if !ctx.token_matches(given) {
            return apply_cors(forbidden());
        }
    }

    let req = Req {
        method: parts.method,
        query: parse_query(parts.uri.query()),
        headers: parts.headers,
        path,
        body,
    };
    apply_cors(route(ctx, req).await)
}

async fn route(ctx: SharedCtx, req: Req) -> Response {
    let post = req.method == Method::POST;
    match req.path.as_str() {
        "/api/audio-output-info" if req.method == Method::GET => {
            json_response(200, &crate::audio_output::system_audio_output_info().await)
        }
        "/api/translate" if post => lyrics::handle_translate(ctx, req).await,
        "/api/state" if post => handle_state(ctx, req).await,
        "/api/lyrics-stream" => handle_lyrics_stream(ctx),
        "/api/download" => download::handle_download(ctx, req).await,
        "/api/tmp-file" => download::handle_tmp_file(ctx, req).await,
        "/api/encode" if post => download::handle_encode(ctx, req).await,
        "/api/spotify-import" => imports::handle_spotify_import(ctx, req).await,
        "/api/apple-import" => imports::handle_apple_import(ctx, req).await,
        "/api/playlist-import" => imports::handle_playlist_import(ctx, req).await,
        "/api/albums/search" => imports::handle_album_search(ctx, req).await,
        "/api/albums/tracks" => imports::handle_album_tracks(ctx, req).await,
        "/api/hot" => recommend::handle_hot_picks(ctx, req).await,
        "/api/spotify-auth-start" => imports::handle_spotify_auth_start(ctx, req),
        "/api/spotify-callback" => imports::handle_spotify_callback(ctx, req).await,
        "/api/spotify-status" => {
            let sp = ctx.spotify.lock();
            json_response(200, &json!({ "connected": !sp.token.is_empty(), "error": sp.error }))
        }
        "/api/spotify-logout" => {
            let mut sp = ctx.spotify.lock();
            sp.token.clear();
            sp.error.clear();
            json_response(200, &json!({ "success": true }))
        }
        "/api/spotify-library" => imports::handle_spotify_library(ctx, req).await,
        "/api/search" => search::handle_search(ctx, req).await,
        "/api/vibe" if post => recommend::handle_vibe(ctx, req).await,
        "/api/artist" => recommend::handle_artist(ctx, req).await,
        "/api/prefetch" => {
            let (url, id, platform) = (req.q("url"), req.q("id"), req.query.get("platform").cloned().filter(|p| !p.is_empty()).unwrap_or_else(|| "youtube".into()));
            let c = ctx.clone();
            tokio::spawn(async move { stream::prefetch_track(c, url, id, platform).await });
            json_response(200, &json!({ "success": true }))
        }
        "/api/check-cache" => stream::handle_check_cache(ctx, req).await,
        "/api/tools-status" => json_response(200, &tools::tools_status(&ctx)),
        "/api/tools-install" => {
            let c = ctx.clone();
            tokio::spawn(async move { tools::ensure_tools(c).await });
            json_response(200, &json!({ "started": true }))
        }
        "/api/stream-info" => stream::handle_stream_info(ctx, req).await,
        "/api/local-track" => media::handle_local_track(ctx, req).await,
        "/api/downloaded-track" => media::handle_downloaded_track(ctx, req).await,
        "/api/library-add" if post => library::handle_library_add(ctx, req).await,
        "/api/library-update" if post => library::handle_library_update(ctx, req).await,
        "/api/library" => library::handle_library_list(ctx),
        "/api/library-file" => media::handle_library_file(ctx, req).await,
        "/api/library-remove" if post => library::handle_library_remove(ctx, req).await,
        "/api/local-thumbnail" => media::handle_local_thumbnail(ctx, req).await,
        "/api/offline-library" => library::handle_offline_library(ctx).await,
        "/api/track-analysis" => library::handle_track_analysis(ctx, req).await,
        "/api/offline-delete" if post => library::handle_offline_delete(ctx, req).await,
        "/api/clear-offline-library" if post => library::handle_clear_offline(ctx).await,
        "/api/audio-proxy" => media::handle_audio_proxy(ctx, req).await,
        "/api/save-custom-lyrics" if post => lyrics::handle_save_custom_lyrics(ctx, req).await,
        "/api/lyrics" => lyrics::handle_lyrics(ctx, req).await,
        "/api/lyrics-offset" => lyrics_sync::handle_lyrics_offset(ctx, req).await,
        _ => text_response(404, "Not found"),
    }
}

async fn handle_state(ctx: SharedCtx, req: Req) -> Response {
    let body = match req.json_body().await {
        Ok(b) => b,
        Err(r) => return r,
    };
    match serde_json::from_str::<Value>(&body) {
        Ok(state) => {
            let msg = format!("data: {}\n\n", state);
            *ctx.app_state.lock() = state;
            let _ = ctx.sse.send(msg);
            json_response(200, &json!({ "success": true }))
        }
        Err(_) => empty_response(400),
    }
}

fn handle_lyrics_stream(ctx: SharedCtx) -> Response {
    let first = format!("data: {}\n\n", ctx.app_state.lock());
    let rx = ctx.sse.subscribe();
    let stream = futures_util::stream::unfold((Some(first), rx), |(pending, mut rx)| async move {
        if let Some(msg) = pending {
            return Some((Ok::<_, std::io::Error>(bytes::Bytes::from(msg)), (None, rx)));
        }
        loop {
            match rx.recv().await {
                Ok(msg) => return Some((Ok(bytes::Bytes::from(msg)), (None, rx))),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .header("access-control-allow-origin", "*")
        .body(Body::from_stream(stream))
        .unwrap()
}

pub async fn start(ctx: SharedCtx, preferred: u16) -> std::io::Result<u16> {
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", preferred)).await {
        Ok(l) => l,
        Err(_) => tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?,
    };
    let port = listener.local_addr()?.port();
    ctx.port.store(port, Ordering::Relaxed);
    let app = Router::new().fallback(entry).with_state(ctx.clone());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            crate::logger::error("HTTP server stopped", e);
        }
    });
    {
        let c = ctx.clone();
        tokio::spawn(async move { media::prune_disk_cache(&c).await });
    }
    {
        let c = ctx.clone();
        tokio::spawn(async move { tools::update_yt_dlp_if_managed(&c).await });
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_rules() {
        assert!(origin_allowed("http://tauri.localhost"));
        assert!(origin_allowed("http://127.0.0.1:5173"));
        assert!(origin_allowed("file://"));
        assert!(!origin_allowed("https://evil.example.com"));
        assert!(host_allowed("127.0.0.1:38472"));
        assert!(host_allowed("[::1]:38472"));
        assert!(!host_allowed("evil.example.com"));
    }
}
