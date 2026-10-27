use super::media::find_cached_file;
use super::textutil::*;
use super::{json_response, stream, ytdlp, Req, SharedCtx, Timed};
use crate::logger;
use axum::response::Response;
use futures_util::{stream as fstream, StreamExt};
use regex::Regex;
use serde_json::{json, Value};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

pub const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const YTM_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";
const YTM_SONGS_PARAMS: &str = "EgWKAQIIAWoMEA4QChADEAQQCRAF";
const YTM_CLIENT_VERSION: &str = "1.20250101.01.00";
const SEARCH_TTL: Duration = Duration::from_secs(300);
const RESULT_TTL: Duration = Duration::from_secs(1800);

static YT_INITIAL_DATA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"var ytInitialData = (\{.*?\});</script>").unwrap());
static YTM_DURATION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+(:\d{1,2}){1,2}$").unwrap());
static YTM_THUMB_SIZE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"=w\d+-h\d+[^&]*$").unwrap());
static VIDEO_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{11}$").unwrap());
static AD_TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?-u:\b)(advert(isement)?|sponsored|ad)(?-u:\b)|реклама").unwrap());
static TOPIC_SUFFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\s*-\s*Topic$").unwrap());
static SONG_OR_VIDEO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(song|video)$").unwrap());

pub fn is_video_id(id: &str) -> bool {
    VIDEO_ID.is_match(id)
}

pub fn strip_topic(s: &str) -> String {
    TOPIC_SUFFIX.replace(s, "").trim().to_string()
}

pub fn cache_get(ctx: &SharedCtx, key: &str) -> Option<Value> {
    let mut c = ctx.caches.lock();
    match c.search.get(key) {
        Some(t) if t.expires > Instant::now() => Some(t.value.clone()),
        Some(_) => {
            c.search.remove(key);
            None
        }
        None => None,
    }
}

pub fn cache_put(ctx: &SharedCtx, key: String, value: Value, ttl: Duration) {
    ctx.caches.lock().search.insert(key, Timed { expires: Instant::now() + ttl, value });
}

pub(crate) async fn music_only_youtube_search(ctx: &SharedCtx, query: &str) -> Vec<Value> {
    let audio_query = format!("{query} audio");
    let (main, audio) = tokio::join!(direct_youtube_search(ctx, query, false), direct_youtube_search(ctx, &audio_query, false));
    let mut seen = std::collections::HashSet::new();
    main.into_iter()
        .chain(audio)
        .filter(|t| !is_video_title(&s(t, "title")))
        .filter(|t| seen.insert(js_string(&t["id"])))
        .collect()
}

pub async fn direct_youtube_search(ctx: &SharedCtx, query: &str, filter_spam: bool) -> Vec<Value> {
    let url = format!("https://www.youtube.com/results?search_query={}", enc(query));
    let html = match ctx
        .http
        .get(&url)
        .header("User-Agent", BROWSER_UA)
        .header("Accept-Language", "en-US,en;q=0.9")
        .timeout(Duration::from_secs(7))
        .send()
        .await
    {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return vec![],
    };
    let Some(m) = YT_INITIAL_DATA.captures(&html).and_then(|c| c.get(1)) else { return vec![] };
    let Ok(data) = serde_json::from_str::<Value>(m.as_str()) else { return vec![] };
    let contents = data
        .pointer("/contents/twoColumnSearchResultsRenderer/primaryContents/sectionListRenderer/contents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut tracks = Vec::new();
    for section in &contents {
        let items = section.pointer("/itemSectionRenderer/contents").and_then(Value::as_array).cloned().unwrap_or_default();
        for item in &items {
            let Some(v) = item.get("videoRenderer") else { continue };
            let vid = s(v, "videoId");
            if vid.is_empty() {
                continue;
            }
            let title = v.pointer("/title/runs/0/text").and_then(Value::as_str).filter(|t| !t.is_empty()).unwrap_or("untitled").to_string();
            let artist = v.pointer("/ownerText/runs/0/text").and_then(Value::as_str).filter(|t| !t.is_empty()).unwrap_or("unknown artist").to_string();
            let dur = parse_duration_seconds(v.pointer("/lengthText/simpleText").and_then(Value::as_str).unwrap_or("0:00"));
            let thumbnail = v
                .pointer("/thumbnail/thumbnails")
                .and_then(Value::as_array)
                .and_then(|t| t.last())
                .map(|t| s(t, "url"))
                .unwrap_or_default();
            if filter_spam && is_spam_or_unofficial(&title, dur, &artist) {
                continue;
            }
            let cached = find_cached_file(ctx, &vid).await.is_some();
            tracks.push(json!({
                "id": vid, "title": title, "artist": artist, "duration": jnum(dur), "thumbnail": thumbnail,
                "url": format!("https://www.youtube.com/watch?v={vid}"), "platform": "youtube", "isCached": cached
            }));
        }
    }
    tracks
}

async fn ytm_search_request(ctx: &SharedCtx, query: &str) -> Result<Value, String> {
    let body = json!({
        "context": { "client": { "clientName": "WEB_REMIX", "clientVersion": YTM_CLIENT_VERSION, "hl": "en", "gl": "US" } },
        "query": query,
        "params": YTM_SONGS_PARAMS
    });
    let resp = ctx
        .http
        .post("https://music.youtube.com/youtubei/v1/search?prettyPrint=false")
        .header("Content-Type", "application/json")
        .header("Origin", "https://music.youtube.com")
        .header("Referer", "https://music.youtube.com/")
        .header("User-Agent", YTM_UA)
        .timeout(Duration::from_secs(8))
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| {
            logger::log("warn", &format!("ytmusic search error: {e}"));
            e.to_string()
        })?;
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if status != 200 {
        logger::log("warn", &format!("ytmusic search status {status}"));
        return Err(format!("ytmusic status {status}"));
    }
    serde_json::from_str(&text).map_err(|e| {
        logger::log("warn", &format!("ytmusic search parse error: {e}"));
        e.to_string()
    })
}

fn parse_ytm_duration(text: &str) -> f64 {
    let t = text.trim();
    if !YTM_DURATION.is_match(t) {
        return 0.0;
    }
    t.split(':').map(|n| n.parse::<f64>().unwrap_or(0.0)).fold(0.0, |acc, n| acc * 60.0 + n)
}

fn parse_ytm_songs(data: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let sections = data
        .pointer("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for section in &sections {
        let items = section.pointer("/musicShelfRenderer/contents").and_then(Value::as_array).cloned().unwrap_or_default();
        for wrap in &items {
            let Some(r) = wrap.get("musicResponsiveListItemRenderer") else { continue };
            let runs_of = |i: usize| -> Vec<String> {
                r.pointer(&format!("/flexColumns/{i}/musicResponsiveListItemFlexColumnRenderer/text/runs"))
                    .and_then(Value::as_array)
                    .map(|runs| runs.iter().map(|x| s(x, "text")).collect())
                    .unwrap_or_default()
            };
            let video_id = r
                .pointer("/playlistItemData/videoId")
                .and_then(Value::as_str)
                .or_else(|| {
                    r.pointer("/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/videoId")
                        .and_then(Value::as_str)
                })
                .unwrap_or("")
                .to_string();
            if !is_video_id(&video_id) {
                continue;
            }
            let title = runs_of(0).join("").trim().to_string();
            let mut segments = Vec::new();
            let mut cur = String::new();
            for run in runs_of(1) {
                if run == " • " {
                    segments.push(cur.trim().to_string());
                    cur.clear();
                } else {
                    cur.push_str(&run);
                }
            }
            segments.push(cur.trim().to_string());
            let mut clean: Vec<String> = segments.into_iter().filter(|s| !s.is_empty() && !SONG_OR_VIDEO.is_match(s)).collect();
            let mut duration = 0.0;
            if let Some(last) = clean.last() {
                let d = parse_ytm_duration(last);
                if d != 0.0 {
                    duration = d;
                    clean.pop();
                }
            }
            let artist = clean.first().map(|a| strip_topic(a)).unwrap_or_default();
            let album = clean.get(1).cloned().unwrap_or_default();
            let thumbs = r
                .pointer("/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut thumbnail = thumbs
                .last()
                .map(|t| s(t, "url"))
                .unwrap_or_else(|| format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg"));
            thumbnail = YTM_THUMB_SIZE.replace(&thumbnail, "=w544-h544").into_owned();
            if title.is_empty() || (duration != 0.0 && duration < 30.0) {
                continue;
            }
            out.push(json!({
                "id": video_id, "title": title,
                "artist": if artist.is_empty() { "Unknown Artist".to_string() } else { artist },
                "album": album, "duration": jnum(duration), "thumbnail": thumbnail
            }));
        }
    }
    out
}

pub async fn search_soundcloud(ctx: &SharedCtx, query: &str, limit: usize) -> Vec<Value> {
    let key = format!("sc:{limit}:{}", query.trim().to_lowercase());
    if let Some(Value::Array(hit)) = cache_get(ctx, &key) {
        return hit;
    }
    let args = vec![
        format!("scsearch{limit}:{query}"),
        "--dump-single-json".into(),
        "--flat-playlist".into(),
        "--skip-download".into(),
        "--no-warnings".into(),
        "--no-check-certificates".into(),
    ];
    let Ok(raw) = ytdlp::execute(&ctx.paths, args, 18_000).await else { return vec![] };
    let Ok(data) = serde_json::from_str::<Value>(&raw) else { return vec![] };
    let entries: Vec<Value> = arr(&data, "entries")
        .iter()
        .map(|item| {
            let id = item.get("id").filter(|v| truthy(v)).map(js_string).unwrap_or_else(|| random_base36(11));
            json!({
                "id": id,
                "title": first_str(&[&item["title"]]).if_empty("untitled"),
                "artist": first_str(&[&item["uploader"], &item["channel"], &item["artist"]]).if_empty("unknown artist"),
                "duration": or_num(item.get("duration"), 0.0),
                "thumbnail": last_thumbnail(item),
                "url": first_str(&[&item["webpage_url"], &item["url"]]),
                "platform": "soundcloud"
            })
        })
        .collect();
    if !entries.is_empty() {
        cache_put(ctx, key, Value::Array(entries.clone()), SEARCH_TTL);
    }
    entries
}

pub async fn search_ytm_tracks(ctx: &SharedCtx, query: &str, limit: usize) -> Vec<Value> {
    let key = format!("ytm_vibe:{limit}:{}", query.trim().to_lowercase());
    if let Some(Value::Array(hit)) = cache_get(ctx, &key) {
        return hit;
    }
    match ytm_search_request(ctx, query).await {
        Ok(data) => {
            let songs: Vec<Value> = parse_ytm_songs(&data).into_iter().take(limit).collect();
            let mut tracks = Vec::new();
            for song in songs {
                let id = s(&song, "id");
                ctx.caches.lock().yt_artist.insert(id.clone(), s(&song, "artist"));
                let dur = num(&song, "duration");
                let thumb = s(&song, "thumbnail");
                tracks.push(json!({
                    "id": id, "title": song["title"], "artist": song["artist"],
                    "duration": jnum(if dur != 0.0 { dur } else { 180.0 }),
                    "thumbnail": if thumb.is_empty() { format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg") } else { thumb },
                    "url": format!("https://www.youtube.com/watch?v={id}"), "platform": "youtube"
                }));
            }
            if !tracks.is_empty() {
                cache_put(ctx, key, Value::Array(tracks.clone()), SEARCH_TTL);
            }
            tracks
        }
        Err(_) => direct_youtube_search(ctx, query, true).await.into_iter().take(limit).collect(),
    }
}

pub trait IfEmpty {
    fn if_empty(self, fallback: &str) -> String;
}

impl IfEmpty for String {
    fn if_empty(self, fallback: &str) -> String {
        if self.is_empty() { fallback.to_string() } else { self }
    }
}

fn schedule_prefetch(ctx: &SharedCtx, tracks: &[Value], platform: &str, delay_ms: u64) {
    if let Some(first) = tracks.first() {
        let (url, id) = (s(first, "url"), s(first, "id"));
        if url.is_empty() {
            return;
        }
        let (c, p) = (ctx.clone(), platform.to_string());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            stream::prefetch_track(c, url, id, p).await;
        });
    }
}

fn extract_artist(item: &Value) -> String {
    let a = match item.get("artists").and_then(Value::as_array) {
        Some(list) if !list.is_empty() => list.iter().map(js_string).collect::<Vec<_>>().join(", "),
        _ => first_str(&[&item["artist"], &item["creator"], &item["channel"], &item["uploader"]]),
    };
    if a.is_empty() { a } else { strip_topic(&a) }
}

fn trip_ytm_breaker(ctx: &SharedCtx) {
    let mut c = ctx.caches.lock();
    c.ytm_fail_count += 1;
    if c.ytm_fail_count >= 3 {
        c.ytm_fail_until = Some(Instant::now() + Duration::from_secs(300));
        logger::log("warn", "ytmusic circuit breaker tripped (3 failures), backing off 5m");
    }
}

async fn search_ytmusic(ctx: &SharedCtx, query: &str, limit: usize, cache_key: &str) -> Response {
    let breaker_open = ctx.caches.lock().ytm_fail_until.map(|u| Instant::now() < u).unwrap_or(false);
    if !breaker_open {
        match ytm_search_request(ctx, query).await {
            Ok(data) => {
                let songs: Vec<Value> = parse_ytm_songs(&data).into_iter().take(limit).collect();
                if !songs.is_empty() {
                    ctx.caches.lock().ytm_fail_count = 0;
                    let mut entries = Vec::new();
                    for song in songs {
                        let id = s(&song, "id");
                        ctx.caches.lock().yt_artist.insert(id.clone(), s(&song, "artist"));
                        let cached = find_cached_file(ctx, &id).await.is_some();
                        entries.push(json!({
                            "id": id, "title": song["title"], "artist": song["artist"], "duration": song["duration"],
                            "thumbnail": song["thumbnail"], "url": format!("https://www.youtube.com/watch?v={id}"),
                            "platform": "youtube", "isCached": cached
                        }));
                    }
                    let result = json!({ "success": true, "tracks": entries });
                    cache_put(ctx, cache_key.to_string(), result.clone(), RESULT_TTL);
                    schedule_prefetch(ctx, result["tracks"].as_array().unwrap(), "youtube", 60);
                    return json_response(200, &result);
                }
                trip_ytm_breaker(ctx);
            }
            Err(_) => trip_ytm_breaker(ctx),
        }
    }

    let args = vec![
        "--flat-playlist".into(),
        "--playlist-end".into(),
        limit.to_string(),
        "-J".into(),
        format!("https://music.youtube.com/search?q={}#songs", enc(query)),
    ];
    let raw = match ytdlp::execute(&ctx.paths, args, 25_000).await {
        Ok(r) => r,
        Err(_) => return json_response(200, &json!({ "success": false, "tracks": [], "error": "YouTube Music is unavailable" })),
    };
    let Ok(data) = serde_json::from_str::<Value>(&raw) else {
        return json_response(200, &json!({ "success": false, "tracks": [], "error": "YouTube Music is unavailable" }));
    };
    let raw_entries: Vec<Value> = arr(&data, "entries")
        .iter()
        .filter(|item| {
            let id = js_string(item.get("id").unwrap_or(&Value::Null));
            let dur = num(item, "duration");
            is_video_id(&id)
                && s(item, "live_status") != "is_live"
                && !(dur != 0.0 && !(30.0..=1200.0).contains(&dur))
                && !AD_TITLE.is_match(&s(item, "title"))
        })
        .take(limit)
        .cloned()
        .collect();

    let mut artists: Vec<String> = vec![String::new(); raw_entries.len()];
    let mut to_fetch = Vec::new();
    for (i, item) in raw_entries.iter().enumerate().take(20) {
        let id = js_string(&item["id"]);
        let flat = extract_artist(item);
        if !flat.is_empty() {
            artists[i] = flat;
        } else if let Some(a) = ctx.caches.lock().yt_artist.get(&id).cloned() {
            artists[i] = a;
        } else {
            to_fetch.push((i, id));
        }
    }
    let fetched: Vec<(usize, Option<String>)> = fstream::iter(to_fetch)
        .map(|(i, id)| {
            let c = ctx.clone();
            async move {
                let args = vec![
                    "--skip-download".into(),
                    "--no-playlist".into(),
                    "--print".into(),
                    "%(artist,artists.0,channel,uploader)s".into(),
                    format!("https://www.youtube.com/watch?v={id}"),
                ];
                let val = ytdlp::execute(&c.paths, args, 10_000).await.ok().and_then(|out| {
                    out.lines().map(str::trim).find(|l| !l.is_empty() && *l != "NA").map(strip_topic).filter(|v| !v.is_empty())
                });
                if let Some(v) = &val {
                    c.caches.lock().yt_artist.insert(id, v.clone());
                }
                (i, val)
            }
        })
        .buffer_unordered(5)
        .collect()
        .await;
    for (i, v) in fetched {
        if let Some(v) = v {
            artists[i] = v;
        }
    }

    let mut entries = Vec::new();
    for (i, item) in raw_entries.iter().enumerate() {
        let id = js_string(&item["id"]);
        let mut thumbnail = last_thumbnail(item);
        if thumbnail.is_empty() {
            thumbnail = format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg");
        }
        let mut title = s(item, "title").if_empty("untitled");
        let mut artist = artists[i].clone();
        if artist.is_empty() {
            artist = ctx.caches.lock().yt_artist.get(&id).cloned().unwrap_or_else(|| extract_artist(item));
        }
        if artist.is_empty() && title.contains(" - ") {
            let parts: Vec<&str> = title.split(" - ").collect();
            artist = parts[0].trim().to_string();
            let rest = parts[1..].join(" - ").trim().to_string();
            if !rest.is_empty() {
                title = rest;
            }
        }
        if artist.is_empty() {
            artist = "Unknown Artist".into();
        }
        let cached = find_cached_file(ctx, &id).await.is_some();
        entries.push(json!({
            "id": id, "title": title, "artist": artist, "duration": or_num(item.get("duration"), 0.0),
            "thumbnail": thumbnail, "url": format!("https://www.youtube.com/watch?v={id}"),
            "platform": "youtube", "isCached": cached
        }));
    }
    let result = json!({ "success": true, "tracks": entries });
    cache_put(ctx, cache_key.to_string(), result.clone(), RESULT_TTL);
    schedule_prefetch(ctx, result["tracks"].as_array().unwrap(), "youtube", 60);
    json_response(200, &result)
}

pub async fn handle_search(ctx: SharedCtx, req: Req) -> Response {
    let query = req.q("q");
    let platform = req.query.get("platform").cloned().filter(|p| !p.is_empty()).unwrap_or_else(|| "youtube".into());
    let limit = req.query.get("limit").filter(|l| !l.is_empty()).map(|l| l.trim().parse::<usize>().unwrap_or(20)).unwrap_or(20);
    let music_only = platform != "ytmusic" && req.q("musicOnly") == "1";
    let cache_key = format!("{platform}:{query}:{limit}{}", if music_only { ":music" } else { "" });
    if let Some(hit) = cache_get(&ctx, &cache_key) {
        return json_response(200, &hit);
    }

    if platform == "youtube" {
        let direct = if music_only { music_only_youtube_search(&ctx, &query).await } else { direct_youtube_search(&ctx, &query, false).await };
        if !direct.is_empty() {
            let tracks: Vec<Value> = direct.into_iter().take(limit).collect();
            let result = json!({ "success": true, "tracks": tracks });
            cache_put(&ctx, cache_key, result.clone(), RESULT_TTL);
            schedule_prefetch(&ctx, result["tracks"].as_array().unwrap(), "youtube", 100);
            return json_response(200, &result);
        }
    }

    if platform == "ytmusic" {
        return search_ytmusic(&ctx, &query, limit, &cache_key).await;
    }

    let fetch = if music_only { limit * 2 } else { limit };
    let prefix = if platform == "soundcloud" { format!("scsearch{fetch}:") } else { format!("ytsearch{fetch}:") };
    let mut args = vec![
        format!("{prefix}{query}"),
        "--dump-single-json".into(),
        "--flat-playlist".into(),
        "--skip-download".into(),
        "--no-warnings".into(),
        "--no-check-certificates".into(),
    ];
    if platform == "youtube" {
        args.extend(ytdlp::strs(&["--extractor-args", "youtube:player_client=android"]));
    }
    let data = match ytdlp::execute(&ctx.paths, args, 25_000).await.and_then(|raw| serde_json::from_str::<Value>(&raw).map_err(|e| e.to_string())) {
        Ok(d) => d,
        Err(e) => return json_response(500, &json!({ "success": false, "error": e })),
    };
    let mut entries = Vec::new();
    for item in arr(&data, "entries") {
        if music_only && is_video_title(&s(item, "title")) {
            continue;
        }
        if entries.len() >= limit {
            break;
        }
        let id = item.get("id").filter(|v| truthy(v)).map(js_string).unwrap_or_else(|| random_base36(11));
        let mut url = first_str(&[&item["webpage_url"], &item["url"]]);
        if platform == "youtube" && url.is_empty() {
            url = format!("https://www.youtube.com/watch?v={}", js_string(&item["id"]));
        }
        let cached = find_cached_file(&ctx, &js_string(&item["id"])).await.is_some();
        entries.push(json!({
            "id": id,
            "title": s(item, "title").if_empty("untitled"),
            "artist": first_str(&[&item["uploader"], &item["channel"], &item["artist"]]).if_empty("unknown artist"),
            "duration": or_num(item.get("duration"), 0.0),
            "thumbnail": last_thumbnail(item),
            "url": url,
            "platform": platform,
            "isCached": cached
        }));
    }
    let result = json!({ "success": true, "tracks": entries });
    cache_put(&ctx, cache_key, result.clone(), RESULT_TTL);
    schedule_prefetch(&ctx, result["tracks"].as_array().unwrap(), &platform, 60);
    json_response(200, &result)
}

async fn suggest_get(ctx: &SharedCtx, url: &str) -> Option<Value> {
    let resp = ctx.http.get(url).header("User-Agent", BROWSER_UA).timeout(Duration::from_millis(3000)).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    serde_json::from_str(&resp.text().await.ok()?).ok()
}

async fn suggest_artists(ctx: &SharedCtx, term: &str) -> Vec<(String, String)> {
    if term.is_empty() {
        return vec![];
    }
    let url = format!("https://api.deezer.com/search/artist?q={}&limit=5", enc(term));
    let Some(data) = suggest_get(ctx, &url).await else { return vec![] };
    data.get("data")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|x| (s(x, "name"), s(x, "picture_medium"))).filter(|(name, _)| !name.is_empty()).collect())
        .unwrap_or_default()
}

pub async fn handle_suggest(ctx: SharedCtx, req: Req) -> Response {
    let query = req.q("q");
    let q = query.trim();
    if q.chars().count() < 2 {
        return json_response(200, &json!({ "success": true, "words": [], "artists": [] }));
    }
    let words_url = format!("https://suggestqueries.google.com/complete/search?client=firefox&ds=yt&q={}", enc(q));
    let words: Vec<String> = suggest_get(&ctx, &words_url)
        .await
        .as_ref()
        .and_then(|v| v.get(1))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).filter(|w| !w.eq_ignore_ascii_case(q)).take(6).map(str::to_string).collect())
        .unwrap_or_default();
    // Searching the top suggested word ("coldplay" for "cold") finds the artist the prefix alone cannot.
    let top = words.first().cloned().unwrap_or_default();
    let (via_word, direct) = tokio::join!(suggest_artists(&ctx, &top), suggest_artists(&ctx, q));

    let ql = q.to_lowercase();
    let wl: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
    let mut seen = std::collections::HashSet::new();
    let artists: Vec<Value> = via_word
        .into_iter()
        .chain(direct)
        .filter(|(name, _)| {
            let n = name.to_lowercase();
            (n.starts_with(&ql) || wl.contains(&n)) && seen.insert(n)
        })
        .take(4)
        .map(|(name, image)| json!({ "name": name, "image": image }))
        .collect();
    let words: Vec<Value> = words.into_iter().map(|w| json!(w)).collect();
    json_response(200, &json!({ "success": true, "words": words, "artists": artists }))
}
