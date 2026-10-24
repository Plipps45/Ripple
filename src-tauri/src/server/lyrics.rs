use super::textutil::*;
use super::{json_response, Req, SharedCtx};
use axum::response::Response;
use regex::Regex;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

const LYRICS_UA: &str = "ripple/2.0";

static BRACKETED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[.*?\]|\(.*?\)").unwrap());
static TITLE_NOISE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)official\s*(music)?\s*video|official\s*audio|lyrics|hd|4k|remastered|visualizer|clip\s*officiel|video\s*clip").unwrap()
});
static FEAT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)ft\.?|feat\.?").unwrap());
static ARTIST_NOISE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)- topic|vevo|records|official").unwrap());
static NON_LYRIC_CHARS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^A-Za-z0-9_\s\x{0400}-\x{04FF}]").unwrap());
static LEGACY_CUSTOM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^A-Za-z0-9_\s-]").unwrap());
static LEGACY_FETCHED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-zA-Z0-9_-]").unwrap());
static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

pub(crate) fn sanitize_for_lyrics(title: &str, artist: &str) -> (String, String) {
    let t = BRACKETED.replace_all(title, "");
    let t = TITLE_NOISE.replace_all(&t, "");
    let t = FEAT.replace_all(&t, "");
    let t = collapse(&NON_LYRIC_CHARS.replace_all(&t, " "));
    let a = BRACKETED.replace_all(artist, "");
    let a = ARTIST_NOISE.replace_all(&a, "");
    let a = collapse(&NON_LYRIC_CHARS.replace_all(&a, " "));
    (t, a)
}

fn lyrics_key(title: &str, artist: &str) -> String {
    let mut h = Sha1::new();
    h.update(format!("{}\u{0}{}", artist.trim().to_lowercase(), title.trim().to_lowercase()).as_bytes());
    hex::encode(h.finalize())[..16].to_string()
}

fn legacy_custom_path(ctx: &SharedCtx, title: &str, artist: &str) -> PathBuf {
    let a = LEGACY_CUSTOM.replace_all(if artist.is_empty() { "unknown" } else { artist }, "_");
    let t = LEGACY_CUSTOM.replace_all(if title.is_empty() { "untitled" } else { title }, "_");
    let name = WHITESPACE.replace_all(&format!("{a}_{t}.lrc").to_lowercase(), "_").into_owned();
    ctx.paths.custom_lyrics_dir.join(name)
}

fn legacy_fetched_path(ctx: &SharedCtx, title: &str, artist: &str) -> PathBuf {
    let t = LEGACY_FETCHED.replace_all(title, "_").to_lowercase();
    let a = LEGACY_FETCHED.replace_all(artist, "_").to_lowercase();
    ctx.paths.lyrics_dir.join(format!("{a}___{t}.lrc"))
}

fn is_ascii(text: &str) -> bool {
    text.is_ascii()
}

async fn lrclib_get(ctx: &SharedCtx, url: &str) -> Option<(u16, String)> {
    let resp = ctx.http.get(url).header("User-Agent", LYRICS_UA).timeout(Duration::from_secs(6)).send().await.ok()?;
    let status = resp.status().as_u16();
    Some((status, resp.text().await.unwrap_or_default()))
}

pub(crate) async fn lrclib_release(ctx: &SharedCtx, title: &str, artist: &str) -> Option<(f64, String)> {
    let (clean_title, clean_artist) = sanitize_for_lyrics(title, artist);
    let api = format!("https://lrclib.net/api/get?track_name={}&artist_name={}", enc(&clean_title), enc(&clean_artist));
    let exact = match lrclib_get(ctx, &api).await {
        Some((200, body)) => serde_json::from_str::<Value>(&body).ok().filter(|v| !s(v, "syncedLyrics").is_empty()),
        _ => None,
    };
    let hit = match exact {
        Some(v) => v,
        None => {
            let query = format!("{clean_artist} {clean_title}").trim().to_string();
            let (_, body) = lrclib_get(ctx, &format!("https://lrclib.net/api/search?q={}", enc(&query))).await?;
            let list: Vec<Value> = serde_json::from_str(&body).ok()?;
            list.into_iter().find(|i| truthy(i.get("syncedLyrics").unwrap_or(&Value::Null)))?
        }
    };
    let d = num(&hit, "duration");
    (d > 0.0).then(|| (d, format!("{} {}", s(&hit, "artistName"), s(&hit, "trackName")).trim().to_string()))
}

async fn fetch_lyrics_direct(ctx: &SharedCtx, title: &str, artist: &str) -> Option<String> {
    let (clean_title, clean_artist) = sanitize_for_lyrics(title, artist);
    let api = format!("https://lrclib.net/api/get?track_name={}&artist_name={}", enc(&clean_title), enc(&clean_artist));
    if let Some((200, body)) = lrclib_get(ctx, &api).await {
        if let Ok(v) = serde_json::from_str::<Value>(&body) {
            let synced = s(&v, "syncedLyrics");
            let plain = s(&v, "plainLyrics");
            if !synced.is_empty() || !plain.is_empty() {
                return Some(if synced.is_empty() { plain } else { synced });
            }
        }
    }
    let query = format!("{clean_artist} {clean_title}").trim().to_string();
    let (_, body) = lrclib_get(ctx, &format!("https://lrclib.net/api/search?q={}", enc(&query))).await?;
    let list: Vec<Value> = serde_json::from_str(&body).ok()?;
    if let Some(with_synced) = list.iter().find(|i| truthy(i.get("syncedLyrics").unwrap_or(&Value::Null))) {
        return Some(s(with_synced, "syncedLyrics"));
    }
    let first = list.first()?;
    let txt = first_str(&[&first["syncedLyrics"], &first["plainLyrics"]]);
    (!txt.is_empty()).then_some(txt)
}

pub async fn handle_lyrics(ctx: SharedCtx, req: Req) -> Response {
    let (title, artist) = (req.q("title"), req.q("artist"));
    let allow_online = req.q("online") != "0" || !req.has_q("online");
    let cache_key = format!("{artist}:{title}");
    if let Some((text, custom)) = ctx.caches.lock().lyrics.get(&cache_key).cloned() {
        return json_response(200, &json!({ "success": true, "lyrics": text, "isCustom": custom }));
    }
    let both = format!("{title}{artist}");

    let mut custom_path = ctx.paths.custom_lyrics_dir.join(format!("{}.lrc", lyrics_key(&title, &artist)));
    if !custom_path.exists() && is_ascii(&both) {
        let legacy = legacy_custom_path(&ctx, &title, &artist);
        if legacy.exists() {
            custom_path = legacy;
        }
    }
    if custom_path.exists() {
        if let Ok(text) = tokio::fs::read_to_string(&custom_path).await {
            ctx.caches.lock().lyrics.insert(cache_key, (text.clone(), true));
            return json_response(200, &json!({ "success": true, "lyrics": text, "isCustom": true }));
        }
    }

    let mut fetched_path = ctx.paths.lyrics_dir.join(format!("{}.lrc", lyrics_key(&title, &artist)));
    if !fetched_path.exists() && is_ascii(&both) {
        let legacy = legacy_fetched_path(&ctx, &title, &artist);
        if legacy.exists() {
            fetched_path = legacy;
        }
    }
    if fetched_path.exists() {
        if let Ok(text) = tokio::fs::read_to_string(&fetched_path).await {
            ctx.caches.lock().lyrics.insert(cache_key, (text.clone(), false));
            return json_response(200, &json!({ "success": true, "lyrics": text, "isCustom": false }));
        }
    }

    if !allow_online {
        return json_response(200, &json!({ "success": true, "lyrics": "" }));
    }
    let raw = fetch_lyrics_direct(&ctx, &title, &artist).await;
    if let Some(text) = raw.as_ref().filter(|t| !t.trim().is_empty()) {
        let _ = tokio::fs::write(&fetched_path, text).await;
    }
    ctx.caches.lock().lyrics.insert(cache_key, (raw.clone().unwrap_or_default(), false));
    json_response(200, &json!({ "success": true, "lyrics": raw.map(Value::String).unwrap_or(Value::Null) }))
}

pub async fn handle_save_custom_lyrics(ctx: SharedCtx, req: Req) -> Response {
    let (title, artist) = (req.q("title"), req.q("artist"));
    let body = match req.json_body().await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let payload = match serde_json::from_str::<Value>(&body) {
        Ok(p) => p,
        Err(e) => return json_response(500, &json!({ "success": false, "error": e.to_string() })),
    };
    let Some(lyrics) = payload.get("lyrics").and_then(Value::as_str) else {
        return json_response(400, &json!({ "success": false, "error": "No lyrics provided" }));
    };
    let path = ctx.paths.custom_lyrics_dir.join(format!("{}.lrc", lyrics_key(&title, &artist)));
    if let Err(e) = tokio::fs::write(&path, lyrics).await {
        return json_response(500, &json!({ "success": false, "error": e.to_string() }));
    }
    ctx.caches.lock().lyrics.remove(&format!("{artist}:{title}"));
    json_response(200, &json!({ "success": true }))
}

async fn call_translate(ctx: &SharedCtx, text: &str, to: &str) -> Option<Vec<String>> {
    for client in ["dict-chrome-ex", "gtx"] {
        let url = format!(
            "https://translate.googleapis.com/translate_a/single?client={client}&sl=auto&tl={}&dt=t&q={}",
            enc(to),
            enc(text)
        );
        let Ok(resp) = ctx.http.get(&url).header("User-Agent", "Mozilla/5.0").timeout(Duration::from_secs(8)).send().await else { continue };
        if resp.status().as_u16() != 200 {
            continue;
        }
        let Ok(data) = resp.json_value().await else { continue };
        if let Some(parts) = data.get(0).and_then(Value::as_array) {
            let joined: String = parts.iter().map(|p| p.get(0).map(js_string).unwrap_or_default()).collect();
            return Some(joined.split('\n').map(str::to_string).collect());
        }
    }
    None
}

trait JsonValue {
    async fn json_value(self) -> Result<Value, ()>;
}

impl JsonValue for reqwest::Response {
    async fn json_value(self) -> Result<Value, ()> {
        let text = self.text().await.map_err(|_| ())?;
        serde_json::from_str(&text).map_err(|_| ())
    }
}

pub async fn handle_translate(ctx: SharedCtx, req: Req) -> Response {
    let body = match req.json_body().await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let payload: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({}));
    let lines_v = payload.get("texts").filter(|v| truthy(v)).or_else(|| payload.get("lines")).cloned().unwrap_or(json!([]));
    let to = first_str(&[&payload["targetLang"], &payload["to"]]);
    let to = if to.is_empty() { "en".to_string() } else { to };
    let lines: Vec<String> = match lines_v.as_array() {
        Some(a) if !a.is_empty() => a.iter().map(js_string).collect(),
        _ => return json_response(200, &json!({ "success": true, "translations": [] })),
    };

    let mut chunks: Vec<Vec<String>> = Vec::new();
    let (mut cur, mut cur_len) = (Vec::new(), 0usize);
    for line in lines {
        let len = line.encode_utf16().count();
        if !cur.is_empty() && cur_len + len + 1 > 1800 {
            chunks.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        cur_len += len + 1;
        cur.push(line);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }

    let mut all = Vec::new();
    for chunk in chunks {
        let joined = chunk.join("\n");
        let mut trans = call_translate(&ctx, &joined, &to).await.unwrap_or_default();
        if trans.len() != chunk.len() {
            trans.clear();
            for line in &chunk {
                if line.trim().is_empty() {
                    trans.push(String::new());
                    continue;
                }
                let single = call_translate(&ctx, line, &to).await;
                trans.push(single.and_then(|v| v.into_iter().next()).filter(|v| !v.is_empty()).unwrap_or_else(|| line.clone()));
            }
        }
        all.extend(trans);
    }
    json_response(200, &json!({ "success": true, "translations": all }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_like_js() {
        let (t, a) = sanitize_for_lyrics("Song (Official Video) [HD]", "Artist - Topic");
        assert_eq!(t, "Song");
        assert_eq!(a, "Artist");
    }

    #[test]
    fn key_is_stable() {
        assert_eq!(lyrics_key("Title", "Artist").len(), 16);
        assert_eq!(lyrics_key(" Title ", "ARTIST"), lyrics_key("title", "artist"));
    }
}
