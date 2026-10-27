use super::search::{strip_topic, BROWSER_UA};
use super::textutil::*;
use super::{json_response, ytdlp, Req, SharedCtx, Timed};
use crate::logger;
use axum::response::Response;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

const SPOTIFY_PATHFINDER_URL: &str = "https://api-partner.spotify.com/pathfinder/v1/query";
const SPOTIFY_FETCH_PLAYLIST_HASH: &str = "8964e8eafb21aa992a7d951d256d83285c04be2105d209262901de70cb97584a";
const FIREFOX_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:125.0) Gecko/20100101 Firefox/125.0";

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).expect($pat));
    };
}

re!(SPOTIFY_PLAYLIST, r"^(?:(?:https://)?open\.spotify\.com/(?:embed/)?playlist/|spotify:playlist:)([a-zA-Z0-9]{22})(?:[/?#]|$)");
re!(NEXT_DATA, r#"<script id="__NEXT_DATA__"[^>]*>([\s\S]*?)</script>"#);
re!(OG_IMAGE, r#"(?i)<meta\s+(?:property|name)=["'](?:og:image|twitter:image)["']\s+content=["']([^"']+)["']"#);
re!(PRIVATE_WORD, r"(?i)private");
re!(APPLE_ALBUM, r"(?i)music\.apple\.com/(?:[a-z]{2}/)?album/(?:[^/]+/)?(\d+)");
re!(APPLE_PLAYLIST, r"(?i)music\.apple\.com/.*?(pl\.(?:u-)?[a-zA-Z0-9_-]+)");
re!(APPLE_SERVER_DATA, r#"(?i)<script\s+type=["']application/json["']\s+id=["']serialized-server-data["'][^>]*>([\s\S]*?)</script>"#);
re!(APPLE_LD_JSON, r#"(?i)<script\s+id=["']schema:music-playlist["']\s+type=["']application/ld\+json["'][^>]*>([\s\S]*?)</script>"#);
re!(APPLE_PRIVATE, r"(?i)private|sign\s*in|login");
re!(APPLE_OG_TITLE, r#"(?i)<meta\s+property=["']og:title["']\s+content=["']([^"']*)["']"#);
re!(HTML_TITLE, r"(?i)<title>([^<]*)</title>");
re!(ON_APPLE_MUSIC, r"(?i)\s*on Apple Music\s*$");
re!(APPLE_TITLE_SUFFIX, r"(?i)\s*-\s*Playlist\s*-\s*Apple Music\s*$");
re!(LEADING_SYMBOLS, r"^[^A-Za-z0-9_\s]+");
re!(ISO_DURATION, r"PT(?:(\d+)M)?(?:(\d+)S)?");
re!(SC_HYDRATION, r"window\.__sc_hydration\s*=\s*(\[[\s\S]*?\]);\s*</script>");
re!(SC_SCRIPT, r#"src="(https://a-v2\.sndcdn\.com/assets/[^"]+\.js)""#);
re!(SC_CLIENT_ID, r#"client_id[:=]["']([a-zA-Z0-9]{32})["']"#);
re!(SINGLE_SUFFIX, r"(?i)\s-\s*Single$");
re!(EP_SUFFIX, r"(?i)\s-\s*EP$");
re!(DASH_SPACED, r"\s+-\s+");
re!(BY_WORD, r"(?i)(?-u:\b)by(?-u:\b)");
re!(NON_ALNUM, r"[^\p{L}\p{N}]");

fn spotify_error(status: u16, code: &str, error: &str) -> Response {
    json_response(status, &json!({ "success": false, "code": code, "error": error }))
}

async fn get_text(ctx: &SharedCtx, url: &str, headers: &[(&str, &str)], timeout: u64) -> Result<(u16, String), String> {
    let mut rb = ctx.http.get(url).timeout(Duration::from_millis(timeout));
    for (k, v) in headers {
        rb = rb.header(*k, *v);
    }
    let resp = rb.send().await.map_err(|e| if e.is_timeout() { "timeout".to_string() } else { e.to_string() })?;
    let status = resp.status().as_u16();
    Ok((status, resp.text().await.unwrap_or_default()))
}

pub async fn fetch_json(ctx: &SharedCtx, url: &str, timeout_ms: u64) -> Result<Value, String> {
    let (status, body) = get_text(ctx, url, &[("User-Agent", "Mozilla/5.0")], timeout_ms).await?;
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    serde_json::from_str(&body).map_err(|e| e.to_string())
}

async fn pathfinder_request(ctx: &SharedCtx, token: &str, variables: Value) -> Result<Value, String> {
    let payload = json!({
        "variables": variables,
        "operationName": "fetchPlaylist",
        "extensions": { "persistedQuery": { "version": 1, "sha256Hash": SPOTIFY_FETCH_PLAYLIST_HASH } }
    });
    let resp = ctx
        .http
        .post(SPOTIFY_PATHFINDER_URL)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("Accept", "text/html,application/json")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Referer", "https://open.spotify.com/")
        .header("User-Agent", BROWSER_UA)
        .timeout(Duration::from_secs(15))
        .body(payload.to_string())
        .send()
        .await
        .map_err(|e| {
            logger::log("error", &format!("api-partner.spotify.com error: {e}"));
            e.to_string()
        })?;
    let status = resp.status().as_u16();
    logger::log(if status == 200 { "warn" } else { "error" }, &format!("api-partner.spotify.com status {status}"));
    let body = resp.text().await.unwrap_or_default();
    if status != 200 {
        return Err(format!("Pathfinder status {status}"));
    }
    serde_json::from_str(&body).map_err(|_| "Invalid pathfinder response".to_string())
}

fn ms_to_s(v: &Value) -> Option<f64> {
    v.as_f64().map(|ms| (ms / 1000.0).round())
}

async fn fetch_full_spotify_playlist(ctx: &SharedCtx, playlist_id: &str, token: &str) -> Result<Option<(Vec<Value>, f64)>, String> {
    let uri = format!("spotify:playlist:{playlist_id}");
    let first = pathfinder_request(ctx, token, json!({ "uri": uri, "offset": 0, "limit": 100, "enableWatchFeedEntrypoint": true })).await?;
    let Some(content) = first.pointer("/data/playlistV2/content") else { return Ok(None) };
    let Some(first_items) = content.get("items").and_then(Value::as_array) else { return Ok(None) };
    let total = content.get("totalCount").filter(|v| truthy(v)).and_then(Value::as_f64).unwrap_or(first_items.len() as f64);
    let mut items = first_items.clone();
    let mut guard = 0;
    while (items.len() as f64) < total && guard < 100 {
        guard += 1;
        let page = pathfinder_request(ctx, token, json!({ "uri": uri, "offset": items.len(), "limit": 100, "enableWatchFeedEntrypoint": true })).await?;
        match page.pointer("/data/playlistV2/content/items").and_then(Value::as_array) {
            Some(p) if !p.is_empty() => items.extend(p.iter().cloned()),
            _ => break,
        }
    }
    let tracks = items
        .iter()
        .filter_map(|it| {
            let t = it.pointer("/itemV2/data").or_else(|| it.pointer("/item/data")).cloned().unwrap_or(json!({}));
            let artists: Vec<String> = t
                .pointer("/albumOfTrack/artists/items")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(|x| x.pointer("/profile/name").map(js_string).unwrap_or_default()).filter(|n| !n.is_empty()).collect())
                .unwrap_or_default();
            let dur = t
                .pointer("/trackDuration/totalMilliseconds")
                .filter(|v| truthy(v))
                .or_else(|| t.pointer("/duration/totalMilliseconds").filter(|v| truthy(v)))
                .or_else(|| t.get("duration_ms"))
                .and_then(ms_to_s)
                .unwrap_or(0.0);
            let title = s(&t, "name").trim().to_string();
            (!title.is_empty()).then(|| json!({ "title": title, "artist": artists.join(", "), "duration": jnum(dur) }))
        })
        .collect();
    Ok(Some((tracks, total)))
}

async fn fetch_spotify_embed(ctx: &SharedCtx, playlist_id: &str, ua: &str) -> Result<(u16, String), String> {
    let url = format!("https://open.spotify.com/embed/playlist/{playlist_id}");
    let r = get_text(
        ctx,
        &url,
        &[("User-Agent", ua), ("Accept-Language", "en-US,en;q=0.9"), ("Accept", "text/html,application/json"), ("Referer", "https://open.spotify.com/")],
        10_000,
    )
    .await;
    match &r {
        Ok((status, _)) => logger::log(if *status == 200 { "warn" } else { "error" }, &format!("open.spotify.com status {status}")),
        Err(e) => logger::log("error", &format!("open.spotify.com error: {e}")),
    }
    r
}

fn walk_spotify(node: &Value, seen: &mut HashSet<String>, tracks: &mut Vec<Value>) {
    match node {
        Value::Array(a) => a.iter().for_each(|i| walk_spotify(i, seen, tracks)),
        Value::Object(_) => {
            let uri = s(node, "uri");
            let title = node.get("title").filter(|v| truthy(v)).or_else(|| node.get("name").filter(|v| truthy(v)));
            let subtitle = if let Some(v) = node.get("subtitle").filter(|v| truthy(v)).or_else(|| node.get("artist").filter(|v| truthy(v))) {
                v.clone()
            } else if let Some(list) = node.get("artists").and_then(Value::as_array) {
                json!(list.iter().map(|a| if a.is_string() { js_string(a) } else { first_str(&[&a["name"], a.pointer("/profile/name").unwrap_or(&Value::Null)]) }).filter(|x| !x.is_empty()).collect::<Vec<_>>().join(", "))
            } else {
                json!(node
                    .pointer("/albumOfTrack/artists/items")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(|x| x.pointer("/profile/name").map(js_string).unwrap_or_default()).filter(|x| !x.is_empty()).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default())
            };
            let duration = node
                .pointer("/duration/totalMilliseconds")
                .filter(|v| truthy(v))
                .or_else(|| node.pointer("/trackDuration/totalMilliseconds").filter(|v| truthy(v)))
                .or_else(|| node.get("duration_ms").filter(|v| truthy(v)))
                .or_else(|| node.get("duration").filter(|v| v.is_number()))
                .cloned()
                .unwrap_or(json!(0));
            if let Some(Value::String(title)) = title {
                let has_sub = truthy(&subtitle);
                let is_track = uri.starts_with("spotify:track:");
                if (has_sub || is_track) && (is_track || s(node, "entityType") == "track" || truthy(&duration)) {
                    let sub = if subtitle.is_string() { js_string(&subtitle) } else { String::new() };
                    let key = format!("{title}__{sub}");
                    if seen.insert(key) {
                        let d = to_num(&duration);
                        let secs = if d > 1000.0 { (d / 1000.0).round() } else { d.round() };
                        tracks.push(json!({ "title": title.trim(), "artist": sub.trim(), "duration": jnum(secs) }));
                    }
                    return;
                }
            }
            if let Value::Object(map) = node {
                for v in map.values() {
                    walk_spotify(v, seen, tracks);
                }
            }
        }
        _ => {}
    }
}

pub async fn handle_spotify_import(ctx: SharedCtx, req: Req) -> Response {
    let input = req.q("url");
    let Some(playlist_id) = SPOTIFY_PLAYLIST.captures(&input).and_then(|c| c.get(1)).map(|m| m.as_str().to_string()) else {
        return spotify_error(400, "spotify_error", "Invalid Spotify playlist URL");
    };

    let embed = match fetch_spotify_embed(&ctx, &playlist_id, BROWSER_UA).await {
        Ok((status, _)) if status == 403 || status == 429 => {
            tokio::time::sleep(Duration::from_millis(800)).await;
            fetch_spotify_embed(&ctx, &playlist_id, FIREFOX_UA).await
        }
        other => other,
    };
    let (status, body) = match embed {
        Ok(r) => r,
        Err(_) => {
            return spotify_error(500, "spotify_error", "Network error connecting to Spotify");
        }
    };

    match status {
        404 => return spotify_error(404, "spotify_not_found", "Playlist not found"),
        403 => {
            return spotify_error(403, "spotify_forbidden", "Spotify refused request (403)");
        }
        429 => return spotify_error(429, "spotify_rate_limited", "Spotify rate limited (429)"),
        200 => {}
        other => {
            return spotify_error(if other == 0 { 500 } else { other }, "spotify_error", &format!("Spotify returned status {other}"));
        }
    }

    let Some(next_data) = NEXT_DATA.captures(&body).and_then(|c| c.get(1)) else {
        let private = PRIVATE_WORD.is_match(&body);
        return spotify_error(403, if private { "spotify_forbidden" } else { "spotify_error" }, if private { "Private playlist" } else { "Could not parse playlist" });
    };
    let Ok(data) = serde_json::from_str::<Value>(next_data.as_str()) else {
        logger::log("error", "Spotify import failed: invalid __NEXT_DATA__");
        return spotify_error(500, "spotify_error", "Internal server error");
    };
    let page_props = data.pointer("/props/pageProps").cloned().unwrap_or(Value::Null);
    if page_props.get("status").and_then(Value::as_i64) == Some(404) {
        return spotify_error(404, "spotify_not_found", "Playlist not found");
    }
    let entity = page_props.pointer("/state/data/entity").cloned().unwrap_or(Value::Null);
    let name = first_str(&[&entity["name"], &page_props["title"]]);
    let name = if name.is_empty() { "Spotify Playlist".to_string() } else { name };
    let og = OG_IMAGE.captures(&body).and_then(|c| c.get(1)).map(|m| m.as_str().to_string()).unwrap_or_default();
    let cover = first_str(&[
        entity.pointer("/images/0/url").unwrap_or(&Value::Null),
        entity.pointer("/coverArt/sources/0/url").unwrap_or(&Value::Null),
        &entity["image"],
        page_props.pointer("/metadata/image").unwrap_or(&Value::Null),
        &json!(og),
    ]);

    let mut tracks: Vec<Value> = Vec::new();
    let mut limited = false;
    let mut total = 0.0;
    let access_token = page_props.pointer("/state/settings/session/accessToken").map(js_string).unwrap_or_default();
    if !access_token.is_empty() {
        match fetch_full_spotify_playlist(&ctx, &playlist_id, &access_token).await {
            Ok(Some((t, tot))) if !t.is_empty() => {
                total = if tot > 0.0 { tot } else { t.len() as f64 };
                tracks = t;
            }
            Ok(_) => {
                limited = true;
                logger::log("warn", "Spotify pathfinder non-200 or empty, falling back to embed trackList");
            }
            Err(e) => {
                limited = true;
                logger::log("warn", &format!("Spotify pathfinder error: {e}, falling back to embed trackList"));
            }
        }
    } else {
        limited = true;
    }

    if tracks.is_empty() {
        let track_list = arr(&entity, "trackList");
        if !track_list.is_empty() {
            tracks = track_list
                .iter()
                .take(100)
                .filter_map(|t| {
                    let dur = t.get("duration").and_then(Value::as_f64).or_else(|| t.get("duration_ms").and_then(Value::as_f64)).map(|ms| (ms / 1000.0).round()).unwrap_or(0.0);
                    let title = first_str(&[&t["title"], &t["name"]]).trim().to_string();
                    (!title.is_empty()).then(|| json!({ "title": title, "artist": first_str(&[&t["subtitle"], &t["artist"]]).trim(), "duration": jnum(dur) }))
                })
                .collect();
        }
        if tracks.is_empty() {
            let mut seen = HashSet::new();
            walk_spotify(&data, &mut seen, &mut tracks);
            tracks.truncate(100);
        }
    }
    if total == 0.0 {
        total = entity.get("totalCount").filter(|v| truthy(v)).and_then(Value::as_f64).unwrap_or_else(|| {
            let n = arr(&entity, "trackList").len();
            if n > 0 { n as f64 } else { tracks.len() as f64 }
        });
    }
    if tracks.is_empty() {
        let private = PRIVATE_WORD.is_match(&body) || PRIVATE_WORD.is_match(&page_props.to_string());
        return spotify_error(403, if private { "spotify_forbidden" } else { "spotify_error" }, if private { "Private playlist" } else { "Could not read tracks" });
    }
    let has_more = total > tracks.len() as f64;
    json_response(200, &json!({ "success": true, "name": name, "cover": cover, "tracks": tracks, "hasMore": has_more, "total": jnum(total), "limited": limited }))
}

async fn fetch_html_with_redirects(ctx: &SharedCtx, url: &str) -> Result<(u16, String), String> {
    get_text(ctx, url, &[("User-Agent", BROWSER_UA), ("Accept-Language", "en-US,en;q=0.9")], 15_000).await
}

fn walk_apple(node: &Value, st: &mut AppleWalk) {
    match node {
        Value::Array(a) => a.iter().for_each(|i| walk_apple(i, st)),
        Value::Object(map) => {
            let kind = node.pointer("/contentDescriptor/kind").map(js_string).unwrap_or_default();
            let typ = s(node, "type");
            let at_type = s(node, "@type");
            if st.title.is_empty() {
                if kind == "playlist" || typ == "playlists" || typ == "playlist" {
                    st.title = first_str(&[&node["title"], &node["name"], node.pointer("/attributes/name").unwrap_or(&Value::Null)]);
                    if let Some(u) = node.pointer("/artwork/dictionary/url").filter(|v| truthy(v)) {
                        st.cover = js_string(u);
                    } else if let Some(u) = node.pointer("/artwork/url").filter(|v| truthy(v)) {
                        st.cover = js_string(u);
                    }
                } else if at_type == "MusicPlaylist" {
                    st.title = s(node, "name");
                }
            }
            if st.total == 0.0 {
                if let Some(n) = node.get("trackCount").and_then(Value::as_f64) {
                    st.total = n;
                }
            }
            let title = if let Some(Value::String(t)) = node.get("title") {
                t.trim().to_string()
            } else if let Some(Value::String(n)) = node.get("name").filter(|_| at_type == "MusicRecording" || typ == "songs") {
                n.trim().to_string()
            } else {
                String::new()
            };
            let artist = match (node.get("artistName"), node.get("artist"), node.pointer("/byArtist/name")) {
                (Some(Value::String(a)), _, _) => a.trim().to_string(),
                (_, Some(Value::String(a)), _) => a.trim().to_string(),
                (_, _, Some(Value::String(a))) => a.trim().to_string(),
                _ => String::new(),
            };
            let dur_raw = node.get("durationInMillis").filter(|v| truthy(v)).or_else(|| node.get("duration").filter(|v| truthy(v))).or_else(|| node.get("duration_ms"));
            let duration = match dur_raw {
                Some(Value::Number(n)) => n.as_f64().map(|d| if d > 1000.0 { (d / 1000.0).round() } else { d.round() }),
                Some(Value::String(t)) if t.starts_with("PT") => ISO_DURATION.captures(t).map(|c| {
                    let m: f64 = c.get(1).map(|x| x.as_str().parse().unwrap_or(0.0)).unwrap_or(0.0);
                    let sec: f64 = c.get(2).map(|x| x.as_str().parse().unwrap_or(0.0)).unwrap_or(0.0);
                    m * 60.0 + sec
                }),
                _ => None,
            };
            let song_like = matches!(kind.as_str(), "song" | "music-video")
                || matches!(typ.as_str(), "songs" | "song" | "library-songs")
                || at_type == "MusicRecording"
                || duration.map(|d| d != 0.0).unwrap_or(false)
                || node.get("audioTraits").map(truthy).unwrap_or(false);
            if !title.is_empty() && !artist.is_empty() && song_like {
                let key = format!("{}___{}", title.to_lowercase(), artist.to_lowercase());
                if st.seen.insert(key) {
                    let mut t = json!({ "title": title, "artist": artist });
                    if let Some(d) = duration {
                        t["duration"] = jnum(d);
                    }
                    st.tracks.push(t);
                }
                return;
            }
            for v in map.values() {
                walk_apple(v, st);
            }
        }
        _ => {}
    }
}

#[derive(Default)]
struct AppleWalk {
    tracks: Vec<Value>,
    seen: HashSet<String>,
    title: String,
    cover: String,
    total: f64,
}

pub async fn handle_apple_import(ctx: SharedCtx, req: Req) -> Response {
    let input = req.q("url");
    if let Some(album_id) = APPLE_ALBUM.captures(&input).and_then(|c| c.get(1)).map(|m| m.as_str().to_string()) {
        if let Ok(data) = fetch_json(&ctx, &format!("https://itunes.apple.com/lookup?id={album_id}&entity=song"), 10_000).await {
            let results = arr(&data, "results");
            if let Some(album) = results.iter().find(|r| s(r, "wrapperType") == "collection") {
                let tracks: Vec<Value> = results
                    .iter()
                    .filter(|r| s(r, "wrapperType") == "track")
                    .map(|it| {
                        json!({
                            "title": s(it, "trackName"),
                            "artist": first_str(&[&it["artistName"], &album["artistName"]]),
                            "duration": jnum(it.get("trackTimeMillis").and_then(ms_to_s).unwrap_or(0.0))
                        })
                    })
                    .collect();
                if !tracks.is_empty() {
                    let name = s(album, "collectionName");
                    return json_response(200, &json!({
                        "success": true,
                        "name": if name.is_empty() { "Album".into() } else { name },
                        "cover": s(album, "artworkUrl100").replacen("100x100", "600x600", 1),
                        "tracks": tracks
                    }));
                }
            }
        }
    }
    if !APPLE_PLAYLIST.is_match(&input) {
        return json_response(400, &json!({ "success": false, "error": "Invalid Apple Music playlist URL" }));
    }
    let (status, body) = match fetch_html_with_redirects(&ctx, &input).await {
        Ok(r) => r,
        Err(e) => {
            logger::error(&format!("Apple import failed: {input}"), &e);
            let net = e == "timeout" || Regex::new(r"(?i)fetch|network|dns|econnrefused|etimedout|connect|error sending").unwrap().is_match(&e);
            let code = if net { 504 } else { 500 };
            return json_response(code, &json!({ "success": false, "error": if net { "network" } else { "unknown" }, "status": code }));
        }
    };
    match status {
        404 => return json_response(404, &json!({ "success": false, "error": "not_found", "status": 404 })),
        429 => return json_response(429, &json!({ "success": false, "error": "rate_limit", "status": 429 })),
        401 | 403 => return json_response(403, &json!({ "success": false, "error": "private", "status": 403 })),
        _ => {}
    }
    let parsed = APPLE_SERVER_DATA
        .captures(&body)
        .and_then(|c| serde_json::from_str::<Value>(c.get(1)?.as_str()).ok())
        .or_else(|| APPLE_LD_JSON.captures(&body).and_then(|c| serde_json::from_str::<Value>(c.get(1)?.as_str()).ok()));
    let Some(parsed) = parsed else {
        let private = APPLE_PRIVATE.is_match(&body);
        return json_response(403, &json!({ "success": false, "error": if private { "private" } else { "restricted" }, "status": 403 }));
    };
    let mut st = AppleWalk::default();
    walk_apple(&parsed, &mut st);
    if st.title.is_empty() {
        if let Some(og) = APPLE_OG_TITLE.captures(&body).and_then(|c| c.get(1)) {
            st.title = ON_APPLE_MUSIC.replace(og.as_str(), "").trim().to_string();
        } else if let Some(t) = HTML_TITLE.captures(&body).and_then(|c| c.get(1)) {
            let x = APPLE_TITLE_SUFFIX.replace(t.as_str(), "");
            st.title = LEADING_SYMBOLS.replace(&x, "").trim().to_string();
        }
    }
    if st.cover.contains("{w}x{h}bb.{f}") {
        st.cover = st.cover.replacen("{w}x{h}bb.{f}", "600x600bb.jpg", 1);
    }
    if st.tracks.is_empty() {
        return json_response(403, &json!({ "success": false, "error": "private", "status": 403 }));
    }
    let total = if st.total > 0.0 { st.total } else { st.tracks.len() as f64 };
    let has_more = total > st.tracks.len() as f64;
    json_response(200, &json!({
        "success": true,
        "name": if st.title.is_empty() { "Apple Music Playlist".to_string() } else { st.title },
        "cover": st.cover, "tracks": st.tracks, "hasMore": has_more, "total": jnum(total)
    }))
}

async fn soundcloud_set(ctx: &SharedCtx, input: &str) -> Option<Value> {
    let (_, page) = get_text(ctx, input, &[("User-Agent", "Mozilla/5.0")], 20_000).await.ok()?;
    let hyd = SC_HYDRATION.captures(&page)?.get(1)?.as_str().to_string();
    let list: Vec<Value> = serde_json::from_str(&hyd).ok()?;
    let pl = list.iter().find(|x| s(x, "hydratable") == "playlist")?.get("data")?.clone();
    let all = pl.get("tracks")?.as_array()?.clone();
    let mut cid = "tHVbSbDP4TaoMbPppngv6Tdry83vNbMQ".to_string();
    for script in SC_SCRIPT.captures_iter(&page).filter_map(|c| c.get(1).map(|m| m.as_str().to_string())) {
        if let Ok((_, code)) = get_text(ctx, &script, &[], 15_000).await {
            if let Some(m) = SC_CLIENT_ID.captures(&code).and_then(|c| c.get(1)) {
                cid = m.as_str().to_string();
                break;
            }
        }
    }
    let mut full: Vec<Value> = all.iter().filter(|t| truthy(t.get("title").unwrap_or(&Value::Null))).cloned().collect();
    let missing: Vec<String> = all.iter().filter(|t| t.is_object() && !truthy(t.get("title").unwrap_or(&Value::Null))).map(|t| js_string(&t["id"])).collect();
    for chunk in missing.chunks(50) {
        let url = format!("https://api-v2.soundcloud.com/tracks?ids={}&client_id={cid}", chunk.join(","));
        if let Ok((_, body)) = get_text(ctx, &url, &[("User-Agent", "Mozilla/5.0")], 15_000).await {
            if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&body) {
                full.extend(items);
            }
        }
    }
    let tracks: Vec<Value> = full
        .iter()
        .map(|t| {
            let user = t.get("user").cloned().unwrap_or(Value::Null);
            json!({
                "id": js_string(&t["id"]),
                "title": first_str(&[&t["title"]]).if_empty_str("untitled"),
                "artist": first_str(&[&user["username"], &user["name"]]).if_empty_str("unknown artist"),
                "duration": jnum((if truthy(&t["duration"]) { to_num(&t["duration"]) } else { to_num(&t["full_duration"]) } / 1000.0).round()),
                "thumbnail": first_str(&[&t["artwork_url"], &user["avatar_url"]]),
                "url": s(t, "permalink_url"),
                "platform": "soundcloud",
                "isCached": false
            })
        })
        .collect();
    if tracks.is_empty() {
        return None;
    }
    Some(json!({ "success": true, "name": first_str(&[&pl["title"]]).if_empty_str("SoundCloud Playlist"), "cover": s(&pl, "artwork_url"), "tracks": tracks }))
}

trait IfEmptyStr {
    fn if_empty_str(self, f: &str) -> String;
}

impl IfEmptyStr for String {
    fn if_empty_str(self, f: &str) -> String {
        if self.is_empty() { f.into() } else { self }
    }
}

pub async fn handle_playlist_import(ctx: SharedCtx, req: Req) -> Response {
    let input = req.q("url");
    let Ok(parsed) = url::Url::parse(&input) else {
        return json_response(400, &json!({ "success": false, "error": "Unsupported link" }));
    };
    let host = parsed.host_str().unwrap_or("").to_lowercase();
    let is_sc = host == "soundcloud.com" || host == "www.soundcloud.com" || host.ends_with(".soundcloud.com");
    let is_yt = matches!(host.as_str(), "youtube.com" | "music.youtube.com" | "www.youtube.com" | "youtu.be") || host.ends_with(".youtube.com");
    if !is_sc && !is_yt {
        return json_response(400, &json!({ "success": false, "error": "Unsupported link" }));
    }
    let platform = if is_sc { "soundcloud" } else if host == "music.youtube.com" { "ytmusic" } else { "youtube" };

    if is_sc && input.contains("/sets/") {
        if let Some(v) = soundcloud_set(&ctx, &input).await {
            return json_response(200, &v);
        }
    }

    let mut args = ytdlp::strs(&["--dump-single-json", "--no-warnings", "--no-check-certificates", "--playlist-end", "5000"]);
    if platform == "youtube" {
        args.insert(0, "--flat-playlist".into());
        args.extend(ytdlp::strs(&["--extractor-args", "youtube:player_client=android"]));
    }
    args.push(input.clone());

    let run = async {
        let raw = ytdlp::execute(&ctx.paths, args.clone(), 60_000).await?;
        let mut data: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        let mut entries: Vec<Value> = data.get("entries").and_then(Value::as_array).cloned().unwrap_or_else(|| vec![data.clone()]);
        if entries.first().map(|e| !truthy(e.get("title").unwrap_or(&Value::Null))).unwrap_or(false) && args.iter().any(|a| a == "--flat-playlist") {
            let deep: Vec<String> = args.iter().filter(|a| *a != "--flat-playlist").cloned().collect();
            let deep_raw = ytdlp::execute(&ctx.paths, deep, 60_000).await?;
            data = serde_json::from_str(&deep_raw).map_err(|e| e.to_string())?;
            entries = data.get("entries").and_then(Value::as_array).cloned().unwrap_or_else(|| vec![data.clone()]);
        }
        Ok::<_, String>((data, entries))
    };
    let (data, entries) = match run.await {
        Ok(r) => r,
        Err(e) => {
            logger::error(&format!("Playlist import failed: {input}"), ytdlp::truncate(&e, 300));
            return json_response(500, &json!({ "success": false, "error": e }));
        }
    };

    let mut tracks = Vec::new();
    for item in &entries {
        if !truthy(item.get("id").unwrap_or(&Value::Null)) {
            continue;
        }
        let title_raw = s(item, "title");
        if title_raw == "[Private video]" || title_raw == "[Deleted video]" {
            continue;
        }
        let id = js_string(&item["id"]);
        let track_url = if platform == "youtube" { format!("https://www.youtube.com/watch?v={id}") } else { first_str(&[&item["webpage_url"], &item["url"]]) };
        if track_url.is_empty() {
            continue;
        }
        let mut title = first_str(&[&item["title"], &item["track"], &item["fulltitle"], &item["name"]]).trim().to_string();
        let artists_joined = item.get("artists").and_then(Value::as_array).map(|a| a.iter().map(js_string).collect::<Vec<_>>().join(", ")).unwrap_or_default();
        let mut artist = first_str(&[&item["artist"], &json!(artists_joined), &item["creator"], &item["channel"], &item["uploader"], &item["album_artist"]]).trim().to_string();
        if !artist.is_empty() {
            artist = strip_topic(&artist);
        }
        if title.contains(" - ") {
            let parts: Vec<&str> = title.split(" - ").collect();
            let head = parts[0].trim().to_string();
            let rest = parts[1..].join(" - ").trim().to_string();
            let al = artist.to_lowercase();
            if artist.is_empty() || al == "unknown artist" || al == "various artists" {
                artist = head;
                if !rest.is_empty() {
                    title = rest;
                }
            } else if head.to_lowercase() == al && !rest.is_empty() {
                title = rest;
            }
        }
        if title.is_empty() {
            title = "untitled".into();
        }
        if artist.is_empty() {
            artist = "unknown artist".into();
        }
        let thumbnail = if platform == "youtube" {
            format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg")
        } else {
            let thumbs = arr(item, "thumbnails");
            let picked = thumbs
                .iter()
                .find(|x| matches!(s(x, "id").as_str(), "t500x500" | "large" | "crop" | "original"))
                .or_else(|| thumbs.last())
                .map(|t| s(t, "url"))
                .unwrap_or_default();
            if picked.is_empty() { first_str(&[&item["thumbnail"], &item["artwork_url"], &item["avatar_url"]]) } else { picked }
        };
        tracks.push(json!({
            "id": id, "title": title, "artist": artist, "duration": jnum(num(item, "duration").round()),
            "thumbnail": thumbnail, "url": track_url, "platform": platform, "isCached": false
        }));
    }
    if tracks.is_empty() {
        return json_response(200, &json!({ "success": false, "error": "No tracks found" }));
    }
    let mut cover = arr(&data, "thumbnails").last().map(|l| if l.is_string() { js_string(l) } else { s(l, "url") }).unwrap_or_default();
    if cover.is_empty() {
        cover = match data.get("thumbnail") {
            Some(Value::String(t)) => t.clone(),
            Some(t @ Value::Object(_)) => s(t, "url"),
            _ => String::new(),
        };
    }
    if cover.is_empty() && tracks.len() == 1 {
        cover = s(&tracks[0], "thumbnail");
    }
    json_response(200, &json!({ "success": true, "name": first_str(&[&data["title"]]).if_empty_str("Imported playlist"), "cover": cover, "tracks": tracks }))
}

fn album_cache_get(ctx: &SharedCtx, key: &str) -> Option<Value> {
    let mut c = ctx.caches.lock();
    match c.album.get(key) {
        Some(t) if Instant::now() <= t.expires => Some(t.value.clone()),
        Some(_) => {
            c.album.remove(key);
            None
        }
        None => None,
    }
}

fn album_cache_set(ctx: &SharedCtx, key: &str, value: Value) {
    let mut c = ctx.caches.lock();
    if c.album.contains_key(key) {
        c.album_order.retain(|k| k != key);
    } else if c.album.len() >= 100 {
        if let Some(old) = c.album_order.pop_front() {
            c.album.remove(&old);
        }
    }
    c.album_order.push_back(key.to_string());
    c.album.insert(key.to_string(), Timed { expires: Instant::now() + Duration::from_secs(900), value });
}

fn album_json(item: &Value) -> Value {
    let date = s(item, "releaseDate");
    json!({
        "id": js_string(&item["collectionId"]),
        "title": s(item, "collectionName"),
        "artist": s(item, "artistName"),
        "year": date.chars().take(4).collect::<String>(),
        "trackCount": jnum(num(item, "trackCount")),
        "cover": s(item, "artworkUrl100").replacen("100x100", "600x600", 1)
    })
}

pub async fn handle_album_search(ctx: SharedCtx, req: Req) -> Response {
    let q = req.q("q").trim().to_string();
    if q.is_empty() {
        return json_response(200, &json!({ "success": true, "albums": [] }));
    }
    let key = format!("search:{}", q.to_lowercase());
    if let Some(hit) = album_cache_get(&ctx, &key) {
        return json_response(200, &hit);
    }
    if let Some(id) = APPLE_ALBUM.captures(&q).and_then(|c| c.get(1)).map(|m| m.as_str().to_string()) {
        if let Ok(data) = fetch_json(&ctx, &format!("https://itunes.apple.com/lookup?id={id}&entity=album"), 10_000).await {
            if let Some(item) = arr(&data, "results").iter().find(|r| s(r, "wrapperType") == "collection") {
                let resp = json!({ "success": true, "albums": [album_json(item)] });
                album_cache_set(&ctx, &key, resp.clone());
                return json_response(200, &resp);
            }
        }
    }
    let clean_q = collapse(&BY_WORD.replace_all(&DASH_SPACED.replace_all(&q, " "), " "));
    let mut urls = vec![
        format!("https://itunes.apple.com/search?term={}&entity=album&media=music&limit=25", enc(&q)),
        format!("https://itunes.apple.com/search?term={}&media=music&limit=25", enc(&q)),
    ];
    if !clean_q.is_empty() && clean_q.to_lowercase() != q.to_lowercase() {
        urls.push(format!("https://itunes.apple.com/search?term={}&entity=album&media=music&limit=25", enc(&clean_q)));
        urls.push(format!("https://itunes.apple.com/search?term={}&media=music&limit=25", enc(&clean_q)));
    }
    let responses = futures_util::future::join_all(urls.iter().map(|u| fetch_json(&ctx, u, 10_000))).await;
    let normalize = |t: &str| NON_ALNUM.replace_all(&t.to_lowercase(), "").into_owned();
    let mut albums = Vec::new();
    let mut seen = HashSet::new();
    for data in responses.into_iter().flatten() {
        for item in arr(&data, "results") {
            if item.get("collectionId").map(|v| v.is_null()).unwrap_or(true) {
                continue;
            }
            let name = s(item, "collectionName");
            if SINGLE_SUFFIX.is_match(&name) {
                continue;
            }
            if num(item, "trackCount") < 4.0 && !EP_SUFFIX.is_match(&name) {
                continue;
            }
            if !seen.insert(format!("{}:{}", normalize(&name), normalize(&s(item, "artistName")))) {
                continue;
            }
            albums.push(album_json(item));
        }
    }
    let lower_q = clean_q.to_lowercase();
    let words: Vec<String> = NON_LETTER_NUM_SPACE
        .replace_all(&lower_q, " ")
        .split_whitespace()
        .filter(|w| w.chars().count() > 1)
        .map(str::to_string)
        .collect();
    let score = |a: &Value| -> i32 {
        let title = s(a, "title").to_lowercase();
        let artist = s(a, "artist").to_lowercase();
        let full = format!("{title} {artist}");
        let mut sc = 0;
        for w in &words {
            if title.contains(w.as_str()) {
                sc += 15;
            } else if artist.contains(w.as_str()) {
                sc += 10;
            }
        }
        if title == lower_q || full == lower_q {
            sc += 50;
        } else if full.contains(&lower_q) || lower_q.contains(&title) {
            sc += 25;
        }
        sc
    };
    albums.sort_by_key(|a| std::cmp::Reverse(score(a)));
    albums.truncate(12);
    let resp = json!({ "success": true, "albums": albums });
    album_cache_set(&ctx, &key, resp.clone());
    json_response(200, &resp)
}

pub async fn handle_album_tracks(ctx: SharedCtx, req: Req) -> Response {
    let id = req.q("id").trim().to_string();
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return json_response(400, &json!({ "success": false, "error": "Invalid album id" }));
    }
    let key = format!("tracks:{id}");
    if let Some(hit) = album_cache_get(&ctx, &key) {
        return json_response(200, &hit);
    }
    let Ok(data) = fetch_json(&ctx, &format!("https://itunes.apple.com/lookup?id={id}&entity=song"), 10_000).await else {
        return json_response(502, &json!({ "success": false, "error": "Album service unavailable" }));
    };
    let results = arr(&data, "results");
    let Some(album_item) = results.iter().find(|r| s(r, "wrapperType") == "collection") else {
        return json_response(404, &json!({ "success": false, "error": "Album not found" }));
    };
    let mut tracks: Vec<Value> = results
        .iter()
        .filter(|r| s(r, "wrapperType") == "track")
        .map(|it| {
            let no = num(it, "trackNumber");
            let disc = num(it, "discNumber");
            json!({
                "no": jnum(if no != 0.0 { no } else { 1.0 }),
                "disc": jnum(if disc != 0.0 { disc } else { 1.0 }),
                "title": s(it, "trackName"), "artist": s(it, "artistName"),
                "durationMs": jnum(num(it, "trackTimeMillis"))
            })
        })
        .collect();
    tracks.sort_by(|a, b| (num(a, "disc"), num(a, "no")).partial_cmp(&(num(b, "disc"), num(b, "no"))).unwrap_or(std::cmp::Ordering::Equal));
    let resp = json!({ "success": true, "album": album_json(album_item), "tracks": tracks });
    album_cache_set(&ctx, &key, resp.clone());
    json_response(200, &resp)
}
