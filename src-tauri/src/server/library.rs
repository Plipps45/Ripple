use super::media::{find_cached_file, is_cache_audio};
use super::textutil::*;
use super::{json_response, Req, SharedCtx};
use axum::response::Response;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::LazyLock;

static LIBRARY_FILE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^local_[a-f0-9]{12}\.[a-z0-9]+$").unwrap());
static LEADING_TRACK_NO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+(?:[.\s-]+)\s*").unwrap());

fn bad_request(e: impl ToString) -> Response {
    json_response(400, &json!({ "success": false, "error": e.to_string() }))
}

async fn json_payload(req: Req) -> Result<Value, Response> {
    let body = req.json_body().await?;
    let src = if body.trim().is_empty() { "{}" } else { body.as_str() };
    serde_json::from_str::<Value>(src).map_err(bad_request)
}

pub async fn handle_library_add(ctx: SharedCtx, req: Req) -> Response {
    let payload = match json_payload(req).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut added = 0;
    for item in arr(&payload, "items") {
        let (id, file) = (s(item, "id"), s(item, "file"));
        if id.is_empty() || file.is_empty() || !LIBRARY_FILE.is_match(&file) || !ctx.paths.library_dir.join(&file).exists() {
            continue;
        }
        let raw_name = s(item, "name").trim().to_string();
        let stripped = LEADING_TRACK_NO.replace(&raw_name, "").trim().to_string();
        let clean = if !stripped.is_empty() { stripped } else if !raw_name.is_empty() { raw_name } else { "Untitled".into() };
        let (mut artist, mut title) = ("Unknown artist".to_string(), clean.clone());
        if let Some(idx) = clean.find(" - ") {
            let a = clean[..idx].trim();
            let t = clean[idx + 3..].trim();
            artist = if a.is_empty() { "Unknown artist".into() } else { a.into() };
            title = if t.is_empty() { clean.clone() } else { t.into() };
        }
        let ext = {
            let e = s(item, "ext");
            if e.is_empty() {
                std::path::Path::new(&file).extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default()
            } else {
                e
            }
        };
        ctx.library.lock().insert(
            id.clone(),
            json!({ "id": id, "file": file, "ext": ext, "title": title, "artist": artist, "duration": 0, "addedAt": now_ms() }),
        );
        added += 1;
    }
    ctx.save_library();
    json_response(200, &json!({ "success": true, "count": added }))
}

pub async fn handle_library_update(ctx: SharedCtx, req: Req) -> Response {
    let payload = match json_payload(req).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let id = s(&payload, "id");
    if let Some(d) = payload.get("duration").and_then(Value::as_f64) {
        let changed = {
            let mut lib = ctx.library.lock();
            match lib.get_mut(&id).and_then(Value::as_object_mut) {
                Some(entry) if !id.is_empty() => {
                    entry.insert("duration".into(), jnum(d));
                    true
                }
                _ => false,
            }
        };
        if changed {
            ctx.save_library();
        }
    }
    json_response(200, &json!({ "success": true }))
}

pub fn handle_library_list(ctx: SharedCtx) -> Response {
    let mut items: Vec<Value> = ctx.library.lock().values().cloned().collect();
    items.sort_by(|a, b| num(b, "addedAt").partial_cmp(&num(a, "addedAt")).unwrap_or(std::cmp::Ordering::Equal));
    let list: Vec<Value> = items
        .iter()
        .map(|it| {
            json!({
                "id": it["id"], "platform": "local", "title": it["title"], "artist": it["artist"],
                "duration": jnum(num(it, "duration")), "thumbnail": ""
            })
        })
        .collect();
    json_response(200, &Value::Array(list))
}

pub async fn handle_library_remove(ctx: SharedCtx, req: Req) -> Response {
    let query_id = req.q("id");
    let payload = match json_payload(req).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let id = Some(s(&payload, "id")).filter(|i| !i.is_empty()).unwrap_or(query_id);
    let removed = if id.is_empty() { None } else { ctx.library.lock().shift_remove(&id) };
    if let Some(item) = removed {
        let file = s(&item, "file");
        if !file.is_empty() {
            let _ = tokio::fs::remove_file(ctx.paths.library_dir.join(file)).await;
        }
        ctx.save_library();
    }
    json_response(200, &json!({ "success": true }))
}

pub async fn handle_offline_library(ctx: SharedCtx) -> Response {
    let mut tracks = Vec::new();
    let mut total: u64 = 0;
    if let Ok(mut rd) = tokio::fs::read_dir(&ctx.paths.cache_dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let file = entry.file_name().to_string_lossy().into_owned();
            if !is_cache_audio(&file) || file.contains(".temp.") {
                continue;
            }
            let clean_id = file.rsplit_once('.').map(|(b, _)| b.to_string()).unwrap_or_else(|| file.clone());
            let Ok(meta_fs) = entry.metadata().await else { continue };
            total += meta_fs.len();
            let meta = ctx.metadata.lock().get(&clean_id).cloned().unwrap_or_else(|| json!({}));
            let thumb = s(&meta, "thumbnail");
            let has_thumb = ctx.paths.thumbnails_dir.join(format!("{clean_id}.jpg")).exists();
            let mtime = meta_fs
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as f64)
                .unwrap_or(0.0);
            let last_played = meta.get("lastPlayed").filter(|v| truthy(v)).cloned().unwrap_or_else(|| jnum(mtime));
            tracks.push(json!({
                "id": Some(s(&meta, "id")).filter(|v| !v.is_empty()).unwrap_or_else(|| clean_id.clone()),
                "cleanId": clean_id,
                "title": Some(s(&meta, "title")).filter(|v| !v.is_empty()).unwrap_or_else(|| clean_id.clone()),
                "artist": Some(s(&meta, "artist")).filter(|v| !v.is_empty()).unwrap_or_else(|| "Unknown artist".into()),
                "duration": or_num(meta.get("duration"), 0.0),
                "thumbnail": thumb,
                "localThumbnail": if has_thumb { format!("{}/api/local-thumbnail?id={clean_id}", ctx.base()) } else { thumb.clone() },
                "platform": Some(s(&meta, "platform")).filter(|v| !v.is_empty()).unwrap_or_else(|| "youtube".into()),
                "fileSize": meta_fs.len(),
                "lastPlayed": last_played,
                "analysis": meta.get("analysis").filter(|v| truthy(v)).cloned().unwrap_or(Value::Null)
            }));
        }
    }
    json_response(200, &json!({ "success": true, "count": tracks.len(), "totalBytes": total, "tracks": tracks }))
}

pub async fn handle_track_analysis(ctx: SharedCtx, req: Req) -> Response {
    if req.method == axum::http::Method::POST {
        let body = match req.json_body().await {
            Ok(b) => b,
            Err(r) => return r,
        };
        let src = if body.trim().is_empty() { "{}" } else { body.as_str() };
        return match serde_json::from_str::<Value>(src) {
            Ok(payload) => {
                let clean = clean_track_id(&s(&payload, "id"));
                if let Some(analysis) = payload.get("analysis").filter(|a| truthy(a)) {
                    if !clean.is_empty() {
                        let mut m = Map::new();
                        m.insert("analysis".into(), analysis.clone());
                        ctx.save_track_metadata(&clean, m);
                    }
                }
                json_response(200, &json!({ "success": true }))
            }
            Err(e) => json_response(400, &json!({ "error": e.to_string() })),
        };
    }
    let clean = clean_track_id(&req.q("id"));
    let analysis = ctx.metadata.lock().get(&clean).and_then(|m| m.get("analysis")).filter(|a| truthy(a)).cloned().unwrap_or(Value::Null);
    json_response(200, &json!({ "success": true, "analysis": analysis }))
}

pub async fn handle_offline_delete(ctx: SharedCtx, req: Req) -> Response {
    let clean = clean_track_id(&req.q("id"));
    if let Some(audio) = find_cached_file(&ctx, &clean).await {
        let _ = tokio::fs::remove_file(audio).await;
    }
    let _ = tokio::fs::remove_file(ctx.paths.thumbnails_dir.join(format!("{clean}.jpg"))).await;
    ctx.metadata.lock().shift_remove(&clean);
    ctx.save_metadata();
    json_response(200, &json!({ "success": true }))
}

pub async fn handle_clear_offline(ctx: SharedCtx) -> Response {
    if let Ok(mut rd) = tokio::fs::read_dir(&ctx.paths.cache_dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            if is_cache_audio(&e.file_name().to_string_lossy()) {
                let _ = tokio::fs::remove_file(e.path()).await;
            }
        }
    }
    if let Ok(mut rd) = tokio::fs::read_dir(&ctx.paths.thumbnails_dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let _ = tokio::fs::remove_file(e.path()).await;
        }
    }
    ctx.metadata.lock().clear();
    let _ = std::fs::write(&ctx.paths.metadata_path, "{}");
    json_response(200, &json!({ "success": true }))
}
