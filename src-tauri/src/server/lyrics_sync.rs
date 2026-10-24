use super::lyrics::{lrclib_release, sanitize_for_lyrics};
use super::media::find_cached_file;
use super::search::music_only_youtube_search;
use super::stream::resolve_args;
use super::textutil::*;
use super::{json_response, ytdlp, Req, SharedCtx};
use crate::{logger, paths};
use axum::response::Response;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

const SAMPLE_RATE: usize = 8000;
const FRAME: usize = SAMPLE_RATE / 20;
const FRAME_SECONDS: f64 = 0.05;
const REFERENCE_SECONDS: usize = 100;
const MAX_INTRO_SECONDS: usize = 90;
const MAX_EARLY_SECONDS: usize = 10;
const MIN_SCORE: f64 = 0.3;
const MIN_MARGIN: f64 = 0.08;

static RESULTS: LazyLock<Mutex<HashMap<String, Value>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub async fn handle_lyrics_offset(ctx: SharedCtx, req: Req) -> Response {
    let id = clean_track_id(&req.q("id"));
    let (title, artist) = (req.q("title"), req.q("artist"));
    if id.is_empty() || title.is_empty() {
        return json_response(400, &json!({ "success": false, "error": "missing track information" }));
    }
    if let Some(hit) = RESULTS.lock().get(&id) {
        return json_response(200, hit);
    }
    let result = match measure_offset(&ctx, &id, &title, &artist).await {
        Ok(v) => v,
        Err(e) => {
            logger::log("warn", &format!("[lyrics-sync] {title}: {e}"));
            return json_response(200, &json!({ "success": false, "error": e }));
        }
    };
    RESULTS.lock().insert(id, result.clone());
    json_response(200, &result)
}

async fn measure_offset(ctx: &SharedCtx, id: &str, title: &str, artist: &str) -> Result<Value, String> {
    let ffmpeg = paths::resolve_binary(&ctx.paths, "ffmpeg").ok_or("ffmpeg is not installed")?;
    let (source, release) = tokio::join!(playing_source(ctx, id), lrclib_release(ctx, title, artist));
    let source = source.ok_or("could not locate the playing audio")?;
    let (release_length, query) = match release {
        Some((length, name)) => (Some(length), name),
        None => {
            let (clean_title, clean_artist) = sanitize_for_lyrics(title, artist);
            (None, format!("{clean_artist} {clean_title}"))
        }
    };
    let candidates = music_only_youtube_search(ctx, &query).await;
    let mut reference = None;
    for t in candidates.iter().filter(|t| js_string(&t["id"]) != id && matches_release(release_length, num(t, "duration"))).take(2) {
        let reference_id = js_string(&t["id"]);
        let target = format!("https://www.youtube.com/watch?v={reference_id}");
        if let Some(url) = ytdlp::execute(&ctx.paths, resolve_args("youtube", &target), 20_000).await.ok().and_then(|raw| ytdlp::first_http_line(&raw)) {
            reference = Some((reference_id, url));
            break;
        }
    }
    if reference.is_none() && release_length.is_some() {
        reference = soundcloud_reference(ctx, &query, release_length).await;
    }
    let (reference_id, reference_url) = reference.ok_or("no audio release found to compare with")?;

    let (upload, release) = tokio::join!(
        envelope(&ffmpeg, &source, REFERENCE_SECONDS + MAX_INTRO_SECONDS),
        envelope(&ffmpeg, &reference_url, REFERENCE_SECONDS)
    );
    let m = best_lag(&upload?, &release?).ok_or("not enough audio to compare")?;
    let offset = (m.lag as f64 * FRAME_SECONDS * 100.0).round() / 100.0;
    let round = |x: f64| (x * 1000.0).round() / 1000.0;
    Ok(json!({
        "success": true,
        "offset": if m.confident() { offset } else { 0.0 },
        "confident": m.confident(),
        "score": round(m.score),
        "margin": round(m.margin),
        "reference": reference_id
    }))
}

fn matches_release(release_length: Option<f64>, duration: f64) -> bool {
    release_length.map(|len| (duration - len).abs() <= 3.0).unwrap_or(true)
}

async fn soundcloud_reference(ctx: &SharedCtx, query: &str, release_length: Option<f64>) -> Option<(String, String)> {
    let args = ytdlp::strs(&[
        "--no-config", "--no-warnings", "-i", "--playlist-items", "1:6", "-f", "bestaudio/best",
        "--print", "%(duration)s\t%(webpage_url)s\t%(url)s", "--",
    ])
    .into_iter()
    .chain([format!("scsearch6:{query}")])
    .collect();
    let raw = ytdlp::execute_partial(&ctx.paths, args, 30_000).await.ok()?;
    raw.lines().find_map(|line| {
        let mut parts = line.trim().splitn(3, '\t');
        let duration = parts.next()?.parse::<f64>().ok()?;
        let (page, url) = (parts.next()?, parts.next()?);
        (matches_release(release_length, duration) && url.starts_with("http")).then(|| (page.to_string(), url.to_string()))
    })
}

async fn playing_source(ctx: &SharedCtx, id: &str) -> Option<String> {
    if let Some(path) = find_cached_file(ctx, id).await {
        return Some(path.to_string_lossy().into_owned());
    }
    let cached = ctx.caches.lock().stream_url.get(&id.to_lowercase()).map(|(_, v)| s(v, "directUrl"));
    if let Some(url) = cached.filter(|u| u.starts_with("http")) {
        return Some(url);
    }
    let raw = ytdlp::execute(&ctx.paths, resolve_args("youtube", &format!("https://www.youtube.com/watch?v={id}")), 20_000).await.ok()?;
    ytdlp::first_http_line(&raw)
}

async fn envelope(ffmpeg: &Path, input: &str, seconds: usize) -> Result<Vec<f64>, String> {
    let mut cmd = ytdlp::command(ffmpeg);
    let secs = seconds.to_string();
    let rate = SAMPLE_RATE.to_string();
    cmd.args(["-nostdin", "-v", "error", "-t", &secs, "-i", input, "-vn", "-ac", "1", "-ar", &rate, "-f", "s16le", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn().map_err(|e| e.to_string())?;
    let out = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .map_err(|_| "decoding timed out".to_string())?
        .map_err(|e| e.to_string())?;
    if !out.status.success() && out.stdout.is_empty() {
        return Err(ytdlp::truncate(&String::from_utf8_lossy(&out.stderr), 200));
    }
    let samples: Vec<f64> = out.stdout.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f64 / 32768.0).collect();
    Ok(samples
        .chunks(FRAME)
        .filter(|c| c.len() == FRAME)
        .map(|c| {
            let rms = (c.iter().map(|x| x * x).sum::<f64>() / FRAME as f64).sqrt();
            (rms + 1e-4).log10()
        })
        .collect())
}

#[derive(Debug, Clone, Copy)]
pub struct Match {
    pub lag: i64,
    pub score: f64,
    pub margin: f64,
}

impl Match {
    pub fn confident(&self) -> bool {
        self.score >= MIN_SCORE && self.margin >= MIN_MARGIN
    }
}

pub fn best_lag(upload: &[f64], release: &[f64]) -> Option<Match> {
    let fps = (1.0 / FRAME_SECONDS) as i64;
    if release.len() < (fps * 20) as usize || upload.len() < (fps * 20) as usize {
        return None;
    }
    let min_overlap = (release.len() as f64 * 0.6) as usize;
    let mut scores: Vec<(i64, f64)> = Vec::new();
    for lag in -(MAX_EARLY_SECONDS as i64 * fps)..=(MAX_INTRO_SECONDS as i64 * fps) {
        let start = (-lag).max(0) as usize;
        let end = release.len().min((upload.len() as i64 - lag).max(0) as usize);
        if end <= start || end - start < min_overlap {
            continue;
        }
        let n = (end - start) as f64;
        let (mut sa, mut sb) = (0.0, 0.0);
        for i in start..end {
            sa += release[i];
            sb += upload[(i as i64 + lag) as usize];
        }
        let (ma, mb) = (sa / n, sb / n);
        let (mut cov, mut va, mut vb) = (0.0, 0.0, 0.0);
        for i in start..end {
            let a = release[i] - ma;
            let b = upload[(i as i64 + lag) as usize] - mb;
            cov += a * b;
            va += a * a;
            vb += b * b;
        }
        if va > 0.0 && vb > 0.0 {
            scores.push((lag, cov / (va.sqrt() * vb.sqrt())));
        }
    }
    let &(lag, score) = scores.iter().max_by(|a, b| a.1.total_cmp(&b.1))?;
    let runner_up = scores.iter().filter(|(l, _)| (l - lag).abs() > fps).map(|&(_, r)| r).fold(f64::MIN, f64::max);
    Some(Match { lag, score, margin: score - runner_up.max(-1.0) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(frames: usize, seed: u64) -> Vec<f64> {
        let mut x = seed;
        (0..frames)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                -2.0 + ((x >> 33) as f64 / (1u64 << 31) as f64)
            })
            .collect()
    }

    #[test]
    fn finds_an_intro_before_the_song() {
        let release = song(2000, 7);
        let mut upload = vec![-4.0; 250];
        upload.extend_from_slice(&release);
        let m = best_lag(&upload, &release[..2000]).unwrap();
        assert_eq!(m.lag, 250);
        assert!(m.score > 0.99);
        assert!(m.confident());
    }

    #[tokio::test]
    #[ignore]
    async fn measures_real_audio() {
        let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
        let ffmpeg = std::path::PathBuf::from(var("RIPPLE_FFMPEG"));
        let upload = envelope(&ffmpeg, &var("RIPPLE_SYNC_UPLOAD"), REFERENCE_SECONDS + MAX_INTRO_SECONDS).await.unwrap();
        let release = envelope(&ffmpeg, &var("RIPPLE_SYNC_RELEASE"), REFERENCE_SECONDS).await.unwrap();
        let m = best_lag(&upload, &release).unwrap();
        let offset = m.lag as f64 * FRAME_SECONDS;
        println!("offset {offset:.2} s, score {:.3}, margin {:.3}, confident {}", m.score, m.margin, m.confident());
        match var("RIPPLE_SYNC_EXPECT").parse::<f64>() {
            Ok(expect) => {
                assert!(m.confident());
                assert!((offset - expect).abs() <= 0.1, "offset {offset}");
            }
            Err(_) => assert!(!m.confident()),
        }
    }

    #[test]
    fn unrelated_audio_is_not_trusted() {
        let m = best_lag(&song(3800, 1), &song(2000, 2)).unwrap();
        assert!(!m.confident());
    }
}
