use rand::Rng;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::LazyLock;

macro_rules! re {
    ($name:ident, $pat:expr) => {
        pub static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).expect($pat));
    };
}

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

pub fn js_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 && f.abs() < 1e15 {
                    format!("{}", f as i64)
                } else {
                    f.to_string()
                }
            } else {
                n.to_string()
            }
        }
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub fn first_str(vals: &[&Value]) -> String {
    vals.iter().find(|v| truthy(v)).map(|v| js_string(v)).unwrap_or_default()
}

pub fn s(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Number(_)) => js_string(&v[key]),
        _ => String::new(),
    }
}

pub fn num(v: &Value, key: &str) -> f64 {
    to_num(v.get(key).unwrap_or(&Value::Null))
}

pub fn to_num(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.trim().parse::<f64>().unwrap_or(0.0),
        Value::Bool(true) => 1.0,
        _ => 0.0,
    }
    .max(f64::MIN)
}

pub fn jnum(f: f64) -> Value {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 9e15 {
        json!(f as i64)
    } else if f.is_finite() {
        json!(f)
    } else {
        json!(0)
    }
}

pub fn or_num(v: Option<&Value>, fallback: f64) -> Value {
    match v {
        Some(x) if truthy(x) && x.is_number() => x.clone(),
        Some(Value::String(s)) if !s.is_empty() => s.parse::<f64>().map(jnum).unwrap_or_else(|_| jnum(fallback)),
        _ => jnum(fallback),
    }
}

pub fn arr<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

pub fn last_thumbnail(item: &Value) -> String {
    let thumbs = arr(item, "thumbnails");
    if let Some(last) = thumbs.last() {
        return s(last, "url");
    }
    s(item, "thumbnail")
}

pub fn random_base36(len: usize) -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut rng = rand::thread_rng();
    (0..len).map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char).collect()
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

re!(NON_ID, r"[^a-zA-Z0-9_-]");

pub fn clean_track_id(id: &str) -> String {
    NON_ID.replace_all(id, "_").into_owned()
}

pub fn parse_duration_seconds(text: &str) -> f64 {
    let parts: Vec<f64> = text.split(':').map(|p| p.trim().parse::<f64>().unwrap_or(f64::NAN)).collect();
    let v = match parts.len() {
        2 => parts[0] * 60.0 + parts[1],
        3 => parts[0] * 3600.0 + parts[1] * 60.0 + parts[2],
        _ => 0.0,
    };
    if v.is_nan() { 0.0 } else { v }
}

static NON_MUSIC_WORDS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)\blecture\b", r"(?i)\btalk\b", r"(?i)\bconversation\b", r"(?i)\bin conversation\b",
        r"(?i)\bacademy\b", r"(?i)\bmasterclass\b", r"(?i)\bdocumentary\b", r"(?i)\bbehind the scenes\b",
        r"(?i)\bmaking of\b", r"(?i)\bexplained\b", r"(?i)\bhistory of\b", r"(?i)\bhow to\b",
        r"(?i)\bnews\b", r"(?i)\blive stream\b", r"(?i)\blivestream\b", r"(?i)\bfull concert\b",
        r"(?i)\bfull set\b", r"(?i)\blive at\b", r"(?i)\bofficial video essay\b",
        r"(?i)\bepisode\s*\d+", r"(?i)\bseason\s*\d+", r"(?i)\bs\d+e\d+\b", r"(?i)\bep\s*\.?\s*\d+\b",
        r"(?i)\bseries\b", r"(?i)\btrailer\b", r"(?i)\bteaser\b", r"(?i)\bfull movie\b",
        r"(?i)\bshort film\b", r"(?i)\bpodcast\b", r"(?i)\bgameplay\b", r"(?i)\bwalkthrough\b",
        r"(?i)\bplaythrough\b", r"(?i)\breview\b", r"(?i)\breaction\b", r"(?i)\binterview\b",
        r"(?i)\baudiobook\b", r"(?i)\bvlog\b", r"(?i)\bunboxing\b", r"(?i)\btutorial\b", r"(?i)\bparody\b",
    ]
    .iter()
    .map(|p| Regex::new(&p.replace(r"\b", r"(?-u:\b)")).expect(p))
    .collect()
});

const BAD_KEYWORDS: &[&str] = &[
    "1 hour", "2 hours", "10 hours", "hour loop", "hours loop", "full album", "discography",
    "ost compilation", "soundtrack compilation", "playlist mix", "dj set", "type beat",
    "slowed + reverb", "slowed down", "sped up", "bass boosted", "nightcore", "reaction to", "reacting to",
];

pub fn is_spam_or_unofficial(title: &str, duration: f64, channel: &str) -> bool {
    if duration != 0.0 && (duration < 90.0 || duration > 420.0) {
        return true;
    }
    let combined = format!("{} {}", title.to_lowercase(), channel.to_lowercase());
    if NON_MUSIC_WORDS.iter().any(|r| r.is_match(&combined)) {
        return true;
    }
    BAD_KEYWORDS.iter().any(|k| combined.contains(k))
}

re!(KEY_OFFICIAL_PAREN, r"(?i)\s*\([^)]*official[^)]*\)");
re!(KEY_BRACKETS, r"(?i)\s*\[[^\]]*\]");
re!(KEY_MEDIA_PAREN, r"(?i)\s*\([^)]*(video|audio|remaster(ed)?|lyrics?|visualizer)[^)]*\)");
re!(KEY_FEAT_INLINE, r"(?i)\s*(feat\.?|ft\.?)\s+[^(\[-]+");
re!(KEY_FEAT_PAREN, r"(?i)\s*\((feat\.?|ft\.?)[^)]*\)");
re!(KEY_ARTIST_FEAT, r"(?i)\s*(feat\.?|ft\.?)\s+.*$");
re!(NON_LETTER_NUM_SPACE, r"[^\p{L}\p{N}\s]");
re!(MULTI_SPACE, r"\s+");

pub fn collapse(text: &str) -> String {
    MULTI_SPACE.replace_all(text, " ").trim().to_string()
}

fn letters_only(text: &str) -> String {
    collapse(&NON_LETTER_NUM_SPACE.replace_all(text, " "))
}

pub fn track_normalized_key(artist: &str, title: &str) -> String {
    let mut a = artist.to_lowercase();
    let mut t = title.to_lowercase();
    t = KEY_OFFICIAL_PAREN.replace_all(&t, "").into_owned();
    t = KEY_BRACKETS.replace_all(&t, "").into_owned();
    t = KEY_MEDIA_PAREN.replace_all(&t, "").into_owned();
    t = KEY_FEAT_INLINE.replace_all(&t, "").into_owned();
    t = KEY_FEAT_PAREN.replace_all(&t, "").into_owned();
    a = KEY_ARTIST_FEAT.replace_all(&a, "").into_owned();
    format!("{}::{}", letters_only(&a), letters_only(&t))
}

pub fn norm_title(title: &str) -> String {
    letters_only(&title.to_lowercase())
}

re!(ARTIST_NOISE, r"(?i)(?-u:\b)(official|vevo|records|topic)(?-u:\b)");
re!(ARTIST_NON_WORD, r"[^A-Za-z0-9_\s&-]");

pub fn normalize_artist_name(name: &str) -> String {
    let a = ARTIST_NOISE.replace_all(name, "");
    let b = ARTIST_NON_WORD.replace_all(&a, " ");
    collapse(&b).to_lowercase()
}

pub fn normalize_track(track: &Value) -> Option<Value> {
    if !track.is_object() {
        return None;
    }
    let title = js_string(track.get("title").unwrap_or(&Value::Null)).trim().to_string();
    let artist = js_string(track.get("artist").unwrap_or(&Value::Null)).trim().to_string();
    let id_src = first_str(&[track.get("id").unwrap_or(&Value::Null), track.get("url").unwrap_or(&Value::Null)]);
    let id = if id_src.is_empty() {
        format!("{}:{}", artist.to_lowercase(), title.to_lowercase())
    } else {
        id_src
    }
    .trim()
    .to_string();
    if id.is_empty() && title.is_empty() {
        return None;
    }
    let field = |k: &str| track.get(k).filter(|v| truthy(v)).cloned().unwrap_or(json!(""));
    Some(json!({
        "id": id,
        "title": if title.is_empty() { "untitled".to_string() } else { title },
        "artist": if artist.is_empty() { "unknown artist".to_string() } else { artist },
        "duration": jnum(num(track, "duration")),
        "thumbnail": field("thumbnail"),
        "url": field("url"),
        "platform": track.get("platform").filter(|v| truthy(v)).cloned().unwrap_or(json!("soundcloud")),
        "searchQuery": field("searchQuery"),
    }))
}

pub fn is_cyrillic(text: &str) -> bool {
    text.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c))
}

re!(FILENAME_BAD, r#"[<>:"/\\|?*]"#);

pub fn sanitize_filename(text: &str) -> String {
    let base = if text.is_empty() { "track" } else { text };
    collapse(&FILENAME_BAD.replace_all(base, ""))
}

pub fn is_safe_media_url(v: &str) -> bool {
    let lower = v.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

pub fn enc(v: &str) -> String {
    urlencoding::encode(v).into_owned()
}

re!(
    VIDEO_TITLE,
    r"(?i)[(\[][^)\]]*\b(v[ií]deos?|videoclips?|clips?|mv|m/v|visuali[sz]ers?)\b[^)\]]*[)\]]|\b(official|music|lyrics?|letra|performance|4k|hd)\s+(v[ií]deo|mv|visuali[sz]er)\b|\bv[ií]deo\s*(oficial|clip)\b|\b(videoclip|m/v)\b"
);

pub fn is_video_title(title: &str) -> bool {
    VIDEO_TITLE.is_match(title)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_titles_are_recognised() {
        for t in [
            "Daft Punk - Get Lucky (Official Video)",
            "KAROL G - Provenza (Video Oficial)",
            "Bad Bunny - Tití Me Preguntó (Vídeo Oficial)",
            "Feid - Normal (Lyric Video)",
            "Song [Official Music Video]",
            "Artista - Tema (Videoclip)",
            "BTS - Dynamite Official MV",
            "Song (M/V)",
            "Song (Official Visualizer)",
        ] {
            assert!(is_video_title(t), "{t}");
        }
        for t in ["Daft Punk - Get Lucky (Official Audio)", "Feid - Normal", "Lana Del Rey - Video Games", "Provenza (Letra)", "Clipse - Grindin'"] {
            assert!(!is_video_title(t), "{t}");
        }
    }

    #[test]
    fn normalized_key_matches_js() {
        assert_eq!(track_normalized_key("Daft Punk feat. Pharrell", "Get Lucky (Official Video) [HD]"), "da::get lucky");
        assert_eq!(track_normalized_key("Кино", "Группа крови"), "кино::группа крови");
        assert_eq!(track_normalized_key("Artist feat. X", "Song ft. Y (Remastered)"), "artist::song");
    }

    #[test]
    fn artist_name_is_ascii_like_js() {
        assert_eq!(normalize_artist_name("Daft Punk - Topic"), "daft punk -");
        assert_eq!(normalize_artist_name("ArtistVEVO Official"), "artistvevo");
    }

    #[test]
    fn spam_filter() {
        assert!(is_spam_or_unofficial("Song 10 hours", 200.0, ""));
        assert!(is_spam_or_unofficial("Short", 30.0, ""));
        assert!(!is_spam_or_unofficial("Artist - Song", 200.0, "Artist"));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_seconds("3:25"), 205.0);
        assert_eq!(parse_duration_seconds("1:02:03"), 3723.0);
        assert_eq!(parse_duration_seconds("x"), 0.0);
    }
}
