use super::artist_bio;
use super::search::{direct_youtube_search, is_video_id, search_soundcloud, search_ytm_tracks};
use super::textutil::*;
use super::{json_response, ytdlp, Req, SharedCtx, Timed};
use axum::response::Response;
use futures_util::future::join_all;
use parking_lot::Mutex;
use rand::Rng;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

static ARTIST_SUFFIXES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i) - Topic|VEVO|Official|Records").unwrap());
static TOPIC_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)-\s*topic$").unwrap());
static ARTIST_DASH_TITLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^.+?\s+-\s+.+$").unwrap());

fn rand_f() -> f64 {
    rand::thread_rng().gen::<f64>()
}

async fn http_get_json(ctx: &SharedCtx, url: &str, timeout_ms: u64, ua: &str) -> Option<Value> {
    let resp = ctx.http.get(url).header("User-Agent", ua).timeout(Duration::from_millis(timeout_ms)).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    serde_json::from_str(&resp.text().await.ok()?).ok()
}

async fn dz_artist_id(ctx: &SharedCtx, name: &str) -> Option<String> {
    let clean = normalize_artist_name(name);
    if clean.is_empty() {
        return None;
    }
    if let Some(t) = ctx.caches.lock().dz_artist_id.get(&clean).filter(|t| Instant::now() < t.expires) {
        return t.value.clone();
    }
    let url = format!("https://api.deezer.com/search/artist?q={}&limit=1", enc(name.trim()));
    let id = http_get_json(ctx, &url, 6000, "Ripple").await.and_then(|d| d.pointer("/data/0/id").filter(|v| truthy(v)).map(js_string));
    ctx.caches.lock().dz_artist_id.insert(clean, Timed { expires: Instant::now() + Duration::from_secs(86_400), value: id.clone() });
    id
}

async fn dz_related_tracks(ctx: &SharedCtx, artist: &str) -> Vec<Value> {
    let Some(id) = dz_artist_id(ctx, artist).await else { return vec![] };
    let Some(rel) = http_get_json(ctx, &format!("https://api.deezer.com/artist/{}/related?limit=12", enc(&id)), 6000, "Ripple").await else { return vec![] };
    let related: Vec<Value> = arr(&rel, "data").iter().take(6).cloned().collect();
    let nested = join_all(related.iter().enumerate().map(|(idx, rel_artist)| async move {
        let rid = js_string(rel_artist.get("id").unwrap_or(&Value::Null));
        if rid.is_empty() {
            return vec![];
        }
        let Some(top) = http_get_json(ctx, &format!("https://api.deezer.com/artist/{}/top?limit=4", enc(&rid)), 6000, "Ripple").await else { return vec![] };
        let m = 1.0 - (idx as f64 / 12.0) * 0.6;
        arr(&top, "data")
            .iter()
            .map(|item| {
                json!({
                    "artist": first_str(&[item.pointer("/artist/name").unwrap_or(&Value::Null), &rel_artist["name"]]).if_empty("unknown artist"),
                    "title": s(item, "title").if_empty("untitled"),
                    "match": m, "source": "deezer"
                })
            })
            .collect::<Vec<_>>()
    }))
    .await;
    nested.into_iter().flatten().collect()
}

async fn dz_radio_tracks(ctx: &SharedCtx, artist: &str) -> Vec<Value> {
    let Some(id) = dz_artist_id(ctx, artist).await else { return vec![] };
    let Some(data) = http_get_json(ctx, &format!("https://api.deezer.com/artist/{}/radio?limit=25", enc(&id)), 6000, "Ripple").await else { return vec![] };
    arr(&data, "data")
        .iter()
        .map(|item| {
            json!({
                "artist": item.pointer("/artist/name").map(js_string).unwrap_or_default().if_empty("unknown artist"),
                "title": s(item, "title").if_empty("untitled"),
                "match": 0.7, "source": "deezer"
            })
        })
        .collect()
}

async fn yt_mix_tracks(ctx: &SharedCtx, seed: &Value) -> Vec<Value> {
    if !matches!(s(seed, "platform").as_str(), "youtube" | "ytmusic") {
        return vec![];
    }
    let mut id = clean_track_id(&s(seed, "id"));
    if id.len() > 11 && (id.starts_with("yt_") || id.starts_with("youtube_")) {
        id = id.trim_start_matches("youtube_").trim_start_matches("yt_").to_string();
    }
    if !is_video_id(&id) {
        return vec![];
    }
    if let Some(t) = ctx.caches.lock().yt_mix.get(&id).filter(|t| Instant::now() < t.expires) {
        return t.value.clone();
    }
    let args = vec![
        "--flat-playlist".into(),
        "--dump-single-json".into(),
        "--no-warnings".into(),
        "--no-check-certificates".into(),
        "--playlist-end".into(),
        "25".into(),
        "--extractor-args".into(),
        "youtube:player_client=android".into(),
        format!("https://www.youtube.com/watch?v={id}&list=RD{id}"),
    ];
    let Ok(raw) = ytdlp::execute(&ctx.paths, args, 25_000).await else { return vec![] };
    let Ok(data) = serde_json::from_str::<Value>(&raw) else { return vec![] };
    let mut results = Vec::new();
    for (index, entry) in arr(&data, "entries").iter().enumerate() {
        let eid = s(entry, "id");
        if eid.is_empty() || eid == id {
            continue;
        }
        let duration = num(entry, "duration");
        if duration > 480.0 {
            continue;
        }
        let artist = first_str(&[&entry["uploader"], &entry["channel"]]).if_empty("unknown artist");
        let title = s(entry, "title").if_empty("untitled");
        if is_spam_or_unofficial(&title, duration, &artist) {
            continue;
        }
        results.push(json!({
            "id": eid, "title": title, "artist": artist, "duration": jnum(duration), "thumbnail": last_thumbnail(entry),
            "url": format!("https://www.youtube.com/watch?v={eid}"), "platform": "youtube", "isCached": false,
            "match": 1.0 - (index as f64 / 25.0) * 0.5, "source": "ytmix", "playable": true
        }));
    }
    ctx.caches.lock().yt_mix.insert(id, Timed { expires: Instant::now() + Duration::from_secs(1800), value: results.clone() });
    results
}

async fn gather_candidates(ctx: &SharedCtx, seeds: &[Value]) -> Vec<Value> {
    let per_seed = join_all(seeds.iter().take(4).map(|seed| async move {
        let weight = Some(num(seed, "weight")).filter(|w| *w != 0.0).unwrap_or(1.0);
        let artist = s(seed, "artist");
        let (ytmix, rel, rad) = tokio::join!(yt_mix_tracks(ctx, seed), dz_related_tracks(ctx, &artist), dz_radio_tracks(ctx, &artist));
        let mut list = Vec::new();
        for mut t in ytmix {
            let m = Some(num(&t, "match")).filter(|m| *m != 0.0).unwrap_or(0.7);
            t["match"] = json!(m * 1.3);
            t["seedWeight"] = json!(weight);
            list.push(t);
        }
        for mut t in rel.into_iter().chain(rad) {
            t["seedWeight"] = json!(weight);
            list.push(t);
        }
        list
    }))
    .await;
    per_seed.into_iter().flatten().collect()
}

fn artist_names(profile: &Value) -> Vec<String> {
    arr(profile, "topArtists").iter().map(|a| if a.is_string() { js_string(a) } else { s(a, "name") }).collect()
}

fn is_cyrillic_profile(profile: &Value) -> bool {
    s(profile, "dominantScript") == "Cyrillic"
}

fn rank_candidates(
    ctx: &SharedCtx,
    cands: &[Value],
    profile: &Value,
    heard_keys: &HashSet<String>,
    avoid_keys: &HashSet<String>,
    avoid_artists: &HashSet<String>,
    limit: usize,
) -> Vec<Value> {
    struct Group {
        key: String,
        norm_artist: String,
        title: String,
        score_sum: f64,
        best: Value,
    }
    let mut order: Vec<String> = Vec::new();
    let mut grouped: HashMap<String, Group> = HashMap::new();
    {
        let caches = ctx.caches.lock();
        for c in cands {
            let (artist, title) = (s(c, "artist"), s(c, "title"));
            let key = track_normalized_key(&artist, &title);
            if heard_keys.contains(&key) || caches.recent_keys.contains(&key) || avoid_keys.contains(&key) {
                continue;
            }
            let norm_artist = normalize_artist_name(&artist);
            if avoid_artists.contains(&norm_artist) {
                continue;
            }
            let weight = Some(num(c, "seedWeight")).filter(|w| *w != 0.0).unwrap_or(1.0);
            let m = Some(num(c, "match")).filter(|m| *m != 0.0).unwrap_or(0.5);
            let val = m * weight;
            match grouped.get_mut(&key) {
                Some(g) => {
                    g.score_sum += val;
                    if truthy(&c["playable"]) && !truthy(&g.best["playable"]) {
                        g.best = c.clone();
                    }
                }
                None => {
                    order.push(key.clone());
                    grouped.insert(key.clone(), Group { key, norm_artist, title, score_sum: val, best: c.clone() });
                }
            }
        }
    }
    let top_names: Vec<String> = artist_names(profile).iter().filter(|n| !n.is_empty()).map(|n| normalize_artist_name(n)).collect();
    let top_set: HashSet<&String> = top_names.iter().collect();
    let cyr = is_cyrillic_profile(profile);
    let mut scored: Vec<(f64, bool, String, Value)> = order
        .iter()
        .filter_map(|k| grouped.remove(k))
        .map(|g| {
            let is_top = top_set.contains(&g.norm_artist) || top_names.iter().any(|a| g.norm_artist.contains(a.as_str()) || a.contains(g.norm_artist.as_str()));
            let mut score = g.score_sum + if is_top { -0.4 } else { 0.6 };
            if cyr && is_cyrillic(&g.title) {
                score += 0.3;
            }
            score += rand_f() * 0.15;
            let mut v = g.best;
            v["key"] = json!(g.key);
            v["normArtist"] = json!(g.norm_artist);
            v["isTop"] = json!(is_top);
            v["rankScore"] = json!(score);
            (score, is_top, g.norm_artist, v)
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let target = limit.max(1) * 2;
    let max_top = ((target as f64 * 0.3).floor() as usize).max(1);
    let mut per_artist: HashMap<String, usize> = HashMap::new();
    let mut top_count = 0;
    let mut result = Vec::new();
    for (_, is_top, norm, v) in scored {
        let cnt = per_artist.get(&norm).copied().unwrap_or(0);
        if cnt >= 2 || (is_top && top_count >= max_top) {
            continue;
        }
        per_artist.insert(norm, cnt + 1);
        if is_top {
            top_count += 1;
        }
        result.push(v);
        if result.len() >= target {
            break;
        }
    }
    result
}

pub async fn resolve_candidate(ctx: &SharedCtx, c: &Value) -> Option<Value> {
    if truthy(&c["playable"]) {
        let mut clean = c.as_object().cloned().unwrap_or_default();
        for k in ["match", "source", "seedWeight", "playable", "key", "normArtist", "isTop", "rankScore"] {
            clean.shift_remove(k);
        }
        return Some(Value::Object(clean));
    }
    let (artist, title) = (s(c, "artist"), s(c, "title"));
    let key = track_normalized_key(&artist, &title);
    if let Some(t) = ctx.caches.lock().resolved.get(&key).filter(|t| Instant::now() < t.expires) {
        return t.value.clone();
    }
    let mut resolved = resolve_on_ytm(ctx, c).await;
    if resolved.is_none() {
        let cand_artist = normalize_artist_name(&artist);
        let cand_title = norm_title(&title);
        let query = format!("{artist} {title}").trim().to_string();
        for r in search_soundcloud(ctx, &query, 4).await {
            let ra = normalize_artist_name(&first_str(&[&r["artist"], &r["uploader"]]));
            let rt = norm_title(&s(&r, "title"));
            let artist_match = ra.contains(&cand_artist) || cand_artist.contains(&ra);
            let title_match = rt.contains(&cand_title) || cand_title.contains(&rt);
            if artist_match && title_match {
                resolved = normalize_track(&r);
                break;
            }
        }
    }
    let mut caches = ctx.caches.lock();
    if caches.resolved.len() >= 500 {
        if let Some(old) = caches.resolved_order.pop_front() {
            caches.resolved.remove(&old);
        }
    }
    if !caches.resolved.contains_key(&key) {
        caches.resolved_order.push_back(key.clone());
    }
    caches.resolved.insert(key, Timed { expires: Instant::now() + Duration::from_secs(86_400), value: resolved.clone() });
    resolved
}

fn pick_weighted_seeds(seeds: &[Value], count: usize) -> Vec<Value> {
    let mut pool: Vec<Value> = seeds.to_vec();
    let mut picked = Vec::new();
    let w = |s: &Value| Some(num(s, "weight")).filter(|w| *w != 0.0).unwrap_or(1.0);
    while picked.len() < count && !pool.is_empty() {
        let total: f64 = pool.iter().map(w).sum();
        let mut r = rand_f() * total;
        let mut idx = 0;
        for (i, sd) in pool.iter().enumerate() {
            r -= w(sd);
            if r <= 0.0 {
                idx = i;
                break;
            }
        }
        picked.push(pool.remove(idx));
    }
    picked
}

fn remember_recommended(ctx: &SharedCtx, track: &Value) {
    let mut c = ctx.caches.lock();
    c.recent_ids.add(s(track, "id"), 100);
    c.recent_keys.add(track_normalized_key(&s(track, "artist"), &s(track, "title")), 100);
}

fn score_discovery(ctx: &SharedCtx, track: &Value, profile: &Value, heard_keys: &HashSet<String>, heard_ids: &HashSet<String>) -> Option<Value> {
    let mut n = normalize_track(track)?;
    let (id, url, artist, title) = (s(&n, "id"), s(&n, "url"), s(&n, "artist"), s(&n, "title"));
    let key = track_normalized_key(&artist, &title);
    if url.is_empty() || id.is_empty() || heard_ids.contains(&id) || heard_keys.contains(&key) {
        return None;
    }
    {
        let c = ctx.caches.lock();
        if c.recent_ids.contains(&id) || c.recent_keys.contains(&key) {
            return None;
        }
    }
    let duration = num(&n, "duration");
    if is_spam_or_unofficial(&title, duration, &artist) {
        return None;
    }
    let mut score = 1.0;
    if TOPIC_END.is_match(&artist) {
        score += 1.5;
    }
    if ARTIST_DASH_TITLE.is_match(&title) {
        score += 1.0;
    }
    let cand = normalize_artist_name(&artist);
    if artist_names(profile).iter().any(|a| cand.contains(a.as_str()) || a.contains(cand.as_str())) {
        score += 3.0;
    }
    let cyr = is_cyrillic(&title);
    if is_cyrillic_profile(profile) && cyr {
        score += 2.0;
    } else if !is_cyrillic_profile(profile) && !cyr {
        score += 1.0;
    }
    let typical = num(profile, "typicalDuration");
    if duration > 0.0 && typical > 0.0 {
        score += (2.0 - (duration - typical).abs() / 60.0).max(0.0);
    }
    score += rand_f() * 1.5;
    n["score"] = json!(score);
    Some(n)
}

fn build_discovery_queries(profile: &Value) -> Vec<String> {
    let mut artists: Vec<String> = artist_names(profile).into_iter().filter(|a| !a.is_empty()).collect();
    let mut rng = rand::thread_rng();
    for i in (1..artists.len()).rev() {
        let j = rng.gen_range(0..=i);
        artists.swap(i, j);
    }
    let mut q: Vec<String> = artists.iter().take(2).cloned().collect();
    if is_cyrillic_profile(profile) && !artists.is_empty() {
        q.push(format!("{} похожие песни", artists[0]));
    }
    if q.is_empty() {
        q.push("synthwave chill".into());
        q.push("indie electronic".into());
    }
    q
}

async fn recommend_vibe_tracks(ctx: &SharedCtx, payload: &Value, limit: usize) -> Value {
    let history: Vec<Value> = arr(payload, "history").iter().filter_map(normalize_track).collect();
    let favorites: Vec<Value> = arr(payload, "favorites").iter().filter_map(normalize_track).collect();
    let client_profile = payload.get("clientProfile").filter(|v| truthy(v)).cloned().unwrap_or(Value::Null);
    let extra_seeds = arr(payload, "seeds").to_vec();
    let has_extra = !extra_seeds.is_empty();
    let refresh = truthy(payload.get("refresh").unwrap_or(&Value::Null));
    let total_user = history.len() + favorites.len();

    let mut heard_ids = HashSet::new();
    let mut heard_keys = HashSet::new();
    for t in history.iter().chain(favorites.iter()) {
        let id = s(t, "id");
        if !id.is_empty() {
            heard_ids.insert(id);
        }
        heard_keys.insert(track_normalized_key(&s(t, "artist"), &s(t, "title")));
    }
    let mut counts: Vec<(String, usize)> = Vec::new();
    let mut cyr_count = 0;
    let mut durations = Vec::new();
    for t in favorites.iter().chain(history.iter()) {
        let a = s(t, "artist");
        if !a.trim().is_empty() {
            let n = normalize_artist_name(&a);
            match counts.iter_mut().find(|(k, _)| *k == n) {
                Some(e) => e.1 += 1,
                None => counts.push((n, 1)),
            }
        }
        if is_cyrillic(&s(t, "title")) {
            cyr_count += 1;
        }
        let d = num(t, "duration");
        if d > 0.0 {
            durations.push(d);
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1));
    let client_top = arr(&client_profile, "topArtists").to_vec();
    let top_artists: Vec<Value> = if !client_top.is_empty() { client_top } else { counts.iter().take(5).map(|(n, _)| json!(n)).collect() };
    let dominant = {
        let d = s(&client_profile, "dominantScript");
        if !d.is_empty() { d } else if cyr_count as f64 > total_user as f64 * 0.35 { "Cyrillic".into() } else { "Latin".into() }
    };
    let typical = {
        let a = num(&client_profile, "avgDuration");
        if a != 0.0 {
            a
        } else if !durations.is_empty() {
            (durations.iter().sum::<f64>() / durations.len() as f64).round()
        } else {
            210.0
        }
    };
    let profile = json!({ "topArtists": top_artists, "dominantScript": dominant, "typicalDuration": jnum(typical), "trackCount": total_user });

    if total_user < 5 && !has_extra {
        return json!({ "profile": profile, "tracks": [] });
    }

    let candidate_seeds: Vec<Value> = if has_extra {
        extra_seeds
            .iter()
            .map(|sd| {
                json!({
                    "artist": s(sd, "artist"), "title": s(sd, "title"), "id": s(sd, "id"),
                    "platform": s(sd, "platform").if_empty("youtube"),
                    "weight": Some(num(sd, "weight")).filter(|w| *w != 0.0).unwrap_or(1.0)
                })
            })
            .collect()
    } else {
        let mut seen = HashSet::new();
        let mut seeds = Vec::new();
        for (list, weight) in [(&favorites, 3.0), (&history, 1.0)] {
            for t in list.iter() {
                let k = track_normalized_key(&s(t, "artist"), &s(t, "title"));
                if seen.insert(k) {
                    seeds.push(json!({
                        "artist": t["artist"], "title": t["title"], "id": t["id"],
                        "platform": s(t, "platform").if_empty("youtube"), "weight": weight
                    }));
                }
            }
        }
        seeds.truncate(8);
        seeds
    };
    let chosen = pick_weighted_seeds(&candidate_seeds, 4);

    let avoid = payload.get("avoid").cloned().unwrap_or(Value::Null);
    let avoid_keys: HashSet<String> = arr(&avoid, "keys").iter().map(js_string).collect();
    let avoid_artists: HashSet<String> = arr(&avoid, "artists").iter().map(|a| normalize_artist_name(&js_string(a))).filter(|a| !a.is_empty()).collect();

    let mut seed_keys: Vec<String> = chosen.iter().map(|sd| track_normalized_key(&s(sd, "artist"), &s(sd, "title"))).collect();
    seed_keys.sort();
    let seed_key = seed_keys.join("|");
    if !refresh && !seed_key.is_empty() {
        let cached = ctx.caches.lock().vibe.get(&seed_key).filter(|t| Instant::now() < t.expires).map(|t| t.value.clone());
        if let Some(tracks) = cached {
            let filtered: Vec<Value> = {
                let c = ctx.caches.lock();
                tracks
                    .into_iter()
                    .filter(|t| !c.recent_ids.contains(&s(t, "id")) && !c.recent_keys.contains(&track_normalized_key(&s(t, "artist"), &s(t, "title"))))
                    .collect()
            };
            if filtered.len() >= limit {
                return json!({ "profile": profile, "tracks": filtered.into_iter().take(limit).collect::<Vec<_>>() });
            }
        }
    }

    let cands = gather_candidates(ctx, &chosen).await;
    let ranked = rank_candidates(ctx, &cands, &profile, &heard_keys, &avoid_keys, &avoid_artists, limit);
    let final_tracks: Mutex<Vec<Value>> = Mutex::new(Vec::new());
    let next = AtomicUsize::new(0);
    let worker = || async {
        loop {
            if final_tracks.lock().len() >= limit {
                break;
            }
            let i = next.fetch_add(1, Ordering::SeqCst);
            let Some(c) = ranked.get(i) else { break };
            if let Some(track) = resolve_candidate(ctx, c).await {
                let id = s(&track, "id");
                let k = track_normalized_key(&s(&track, "artist"), &s(&track, "title"));
                let recent = {
                    let cc = ctx.caches.lock();
                    cc.recent_ids.contains(&id) || cc.recent_keys.contains(&k)
                };
                let mut ft = final_tracks.lock();
                if !ft.iter().any(|t| s(t, "id") == id) && !heard_ids.contains(&id) && !heard_keys.contains(&k) && !recent && ft.len() < limit {
                    ft.push(track);
                }
            }
        }
    };
    let concurrency = ranked.len().min(4);
    join_all((0..concurrency).map(|_| worker())).await;
    let final_tracks = final_tracks.into_inner();

    if !final_tracks.is_empty() {
        for t in &final_tracks {
            remember_recommended(ctx, t);
        }
        if !seed_key.is_empty() {
            ctx.caches.lock().vibe.insert(seed_key, Timed { expires: Instant::now() + Duration::from_secs(1200), value: final_tracks.clone() });
        }
        return json!({ "profile": profile, "tracks": final_tracks });
    }

    let mut queries = if artist_names(&profile).is_empty() { vec![] } else { build_discovery_queries(&profile) };
    if queries.is_empty() {
        queries = vec!["Top Hits 2025".into(), "Trending Music".into(), "Pop Hits".into()];
    }
    let results = join_all(queries.iter().take(3).map(|q| search_ytm_tracks(ctx, q, 16))).await;
    let mut all_found = Vec::new();
    let mut cand_order: Vec<String> = Vec::new();
    let mut cand_map: HashMap<String, Value> = HashMap::new();
    for list in results {
        for track in list {
            all_found.push(track.clone());
            if let Some(scored) = score_discovery(ctx, &track, &profile, &heard_keys, &heard_ids) {
                let id = s(&scored, "id");
                let better = cand_map.get(&id).map(|e| num(e, "score") < num(&scored, "score")).unwrap_or(true);
                if better {
                    if !cand_map.contains_key(&id) {
                        cand_order.push(id.clone());
                    }
                    cand_map.insert(id, scored);
                }
            }
        }
    }
    if cand_map.len() < limit {
        for track in &all_found {
            let id = s(track, "id");
            if !cand_map.contains_key(&id) {
                let mut t = track.clone();
                t["score"] = json!(50);
                cand_order.push(id.clone());
                cand_map.insert(id, t);
            }
            if cand_map.len() >= limit * 2 {
                break;
            }
        }
    }
    let mut ranked: Vec<Value> = cand_order.iter().filter_map(|k| cand_map.remove(k)).collect();
    ranked.sort_by(|a, b| num(b, "score").partial_cmp(&num(a, "score")).unwrap_or(std::cmp::Ordering::Equal));
    let unshown: Vec<Value> = {
        let c = ctx.caches.lock();
        ranked.iter().filter(|t| !c.recent_ids.contains(&s(t, "id"))).cloned().collect()
    };
    if unshown.len() >= limit {
        ranked = unshown;
    }
    let mut per_artist: HashMap<String, usize> = HashMap::new();
    let mut fallback: Vec<Value> = Vec::new();
    for track in ranked {
        let a = normalize_artist_name(&s(&track, "artist"));
        let cnt = per_artist.get(&a).copied().unwrap_or(0);
        if cnt < 3 {
            per_artist.insert(a, cnt + 1);
            remember_recommended(ctx, &track);
            fallback.push(track);
            if fallback.len() >= limit {
                break;
            }
        }
    }
    if fallback.len() < limit {
        for track in all_found {
            if !fallback.iter().any(|t| s(t, "id") == s(&track, "id")) {
                fallback.push(track);
                if fallback.len() >= limit {
                    break;
                }
            }
        }
    }
    json!({ "profile": profile, "tracks": fallback })
}

pub async fn handle_vibe(ctx: SharedCtx, req: Req) -> Response {
    let body = match req.json_body().await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let payload: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({}));
    let raw_limit = payload.get("limit").filter(|v| truthy(v)).map(to_num).unwrap_or(10.0).trunc();
    let limit = (if raw_limit == 0.0 { 10.0 } else { raw_limit }).clamp(4.0, 16.0) as usize;
    let mut result = recommend_vibe_tracks(&ctx, &payload, limit).await;
    let mut out = Map::new();
    out.insert("success".into(), json!(true));
    if let Value::Object(m) = result.take() {
        out.extend(m);
    }
    json_response(200, &Value::Object(out))
}

async fn fetch_artist_info(ctx: &SharedCtx, artist_name: &str) -> Value {
    let clean = ARTIST_SUFFIXES.replace_all(artist_name, "").trim().to_string();
    let key = clean.to_lowercase();
    if let Some(hit) = ctx.caches.lock().artist.get(&key).cloned() {
        return hit;
    }
    let mut avatar = String::new();
    let mut verified = false;
    let mut tracks: Vec<Value> = Vec::new();
    let mut resolved_name = clean.clone();

    let dz_url = format!("https://api.deezer.com/search/artist?q={}", enc(&clean));
    let (dz, bio) = tokio::join!(
        http_get_json(ctx, &dz_url, 4000, "Mozilla/5.0"),
        artist_bio::lookup(ctx, &clean)
    );
    if let Some(dz) = dz {
        if let Some(top) = arr(&dz, "data").first() {
            let n = s(top, "name");
            if !n.is_empty() {
                resolved_name = n;
            }
            avatar = first_str(&[&top["picture_xl"], &top["picture_big"], &top["picture_medium"]]);
            verified = num(top, "nb_fan") > 30_000.0;
            let tracklist = s(top, "tracklist");
            if !tracklist.is_empty() {
                if let Some(tl) = http_get_json(ctx, &tracklist, 4000, "Mozilla/5.0").await {
                    for item in arr(&tl, "data") {
                        let dur = Some(num(item, "duration")).filter(|d| *d != 0.0).unwrap_or(180.0);
                        let title = s(item, "title");
                        if (50.0..=450.0).contains(&dur) && !is_spam_or_unofficial(&title, dur, "") {
                            let search_query = format!("{clean} - {title} official audio");
                            tracks.push(json!({
                                "id": format!("dz_{}", js_string(&item["id"])),
                                "title": title.clone().if_empty("untitled"),
                                "artist": item.pointer("/artist/name").map(js_string).unwrap_or_default().if_empty(&clean),
                                "duration": jnum(dur),
                                "thumbnail": first_str(&[item.pointer("/album/cover_medium").unwrap_or(&Value::Null), item.pointer("/album/cover_big").unwrap_or(&Value::Null), &json!(avatar)]),
                                "url": format!("https://www.youtube.com/results?search_query={}", enc(&search_query)),
                                "searchQuery": search_query,
                                "platform": "youtube",
                                "isCached": false
                            }));
                        }
                    }
                }
            }
        }
    }
    let bio = bio.map(|b| {
        if avatar.is_empty() {
            avatar = b.image;
        }
        b.text
    }).unwrap_or_default();
    if tracks.is_empty() {
        tracks = direct_youtube_search(ctx, &format!("{clean} official audio"), true)
            .await
            .into_iter()
            .filter(|t| !is_spam_or_unofficial(&s(t, "title"), num(t, "duration"), ""))
            .take(25)
            .collect();
    }
    if avatar.is_empty() {
        avatar = tracks.first().map(|t| s(t, "thumbnail")).unwrap_or_default();
    }
    tracks.truncate(35);
    let result = json!({ "name": resolved_name, "bio": bio.trim(), "avatar": avatar, "isVerified": verified, "tracks": tracks });
    if !s(&result, "bio").is_empty() {
        ctx.caches.lock().artist.insert(key, result.clone());
    }
    result
}

pub async fn handle_artist(ctx: SharedCtx, req: Req) -> Response {
    let name = req.q("name");
    if name.is_empty() {
        return json_response(400, &json!({ "success": false, "error": "Missing artist name" }));
    }
    let info = fetch_artist_info(&ctx, &name).await;
    json_response(200, &json!({ "success": true, "artist": info }))
}

async fn resolve_on_ytm(ctx: &SharedCtx, c: &Value) -> Option<Value> {
    let (artist, title) = (s(c, "artist"), s(c, "title"));
    let cand_artist = normalize_artist_name(&artist);
    let cand_title = norm_title(&title);
    search_ytm_tracks(ctx, &format!("{artist} {title}"), 5).await.into_iter().find(|r| {
        let ra = normalize_artist_name(&s(r, "artist"));
        let rt = norm_title(&s(r, "title"));
        let artist_match = !ra.is_empty() && (ra.contains(&cand_artist) || cand_artist.contains(&ra));
        let title_match = !rt.is_empty() && (rt.contains(&cand_title) || cand_title.contains(&rt));
        artist_match && title_match
    })
}

pub async fn handle_hot_picks(ctx: SharedCtx, req: Req) -> Response {
    let limit = req.q("limit").trim().parse::<i64>().ok().filter(|l| *l != 0).unwrap_or(12).clamp(1, 25) as usize;
    let cached = ctx.caches.lock().hot_picks.as_ref().filter(|t| Instant::now() < t.expires).map(|t| t.value.clone());
    if let Some(data) = cached {
        let tracks: Vec<Value> = arr(&data, "tracks").iter().take(limit).cloned().collect();
        return json_response(200, &json!({ "success": true, "title": data["title"], "tracks": tracks }));
    }
    let Some(chart) = http_get_json(&ctx, "https://api.deezer.com/chart/0/tracks?limit=40", 10_000, "Mozilla/5.0").await else {
        return json_response(200, &json!({ "success": false }));
    };
    let candidates: Vec<Value> = arr(&chart, "data")
        .iter()
        .filter(|i| !s(i, "title").is_empty() && i.pointer("/artist/name").map(truthy).unwrap_or(false))
        .map(|i| json!({ "artist": i.pointer("/artist/name").map(js_string).unwrap_or_default(), "title": s(i, "title") }))
        .collect();
    if candidates.is_empty() {
        return json_response(200, &json!({ "success": false }));
    }
    let resolved: Mutex<Vec<(usize, Value)>> = Mutex::new(Vec::new());
    let seen: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
    let next = AtomicUsize::new(0);
    let worker = || async {
        loop {
            if resolved.lock().len() >= 12 {
                break;
            }
            let i = next.fetch_add(1, Ordering::SeqCst);
            let Some(c) = candidates.get(i) else { break };
            if let Some(track) = resolve_candidate(&ctx, c).await {
                let id = s(&track, "id");
                if seen.lock().insert(id) {
                    let mut r = resolved.lock();
                    if r.len() < 12 {
                        r.push((i, track));
                    }
                }
            }
        }
    };
    join_all((0..6).map(|_| worker())).await;
    let mut resolved = resolved.into_inner();
    resolved.sort_by_key(|(i, _)| *i);
    let tracks: Vec<Value> = resolved.into_iter().map(|(_, t)| t).collect();
    if tracks.is_empty() {
        return json_response(200, &json!({ "success": false }));
    }
    let title = format!("Hot picks {}", chrono::Local::now().format("%Y"));
    ctx.caches.lock().hot_picks = Some(Timed {
        expires: Instant::now() + Duration::from_secs(3600),
        value: json!({ "title": title, "tracks": tracks }),
    });
    json_response(200, &json!({ "success": true, "title": title, "tracks": tracks.into_iter().take(limit).collect::<Vec<_>>() }))
}

trait IfEmpty {
    fn if_empty(self, fallback: &str) -> String;
}

impl IfEmpty for String {
    fn if_empty(self, fallback: &str) -> String {
        if self.is_empty() { fallback.to_string() } else { self }
    }
}
