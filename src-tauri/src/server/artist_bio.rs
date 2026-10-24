use super::textutil::*;
use super::SharedCtx;
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

const UA: &str = "Ripple/1.0 (desktop music player)";

static MUSIC_SUBJECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(band|singer|rapper|musician|songwriter|composer|vocalist|guitarist|bassist|drummer|pianist|violinist|cellist|dj|disc jockey|duo|trio|orchestra|ensemble|choir|music|musical|recording artist|record producer|music producer)s?\b").unwrap()
});
static NOT_AN_ARTIST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:sub)?genre\b|\bstyle of music\b|\bmusic(?:al)? style\b|\balbum\b|\bsong\b|\bsingle by\b|\brecord label\b|\bfestival\b|\binstrument\b").unwrap()
});
static PRONUNCIATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s*\((?:[^()]*(?:pronunciation|[ˈˌəɪʊʃʒθðŋɛɔæ])[^()]*|\s*;[^()]*|\s*)\)").unwrap()
});
static IS_A_GENRE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bis an? (?:\S+ ){0,3}(?:sub)?genre\b").unwrap());
static NON_ALNUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^\p{L}\p{N}]+").unwrap());

pub struct ArtistBio {
    pub text: String,
    pub image: String,
}

async fn get_json(ctx: &SharedCtx, url: &str, timeout_ms: u64) -> Option<Value> {
    let resp = ctx.http.get(url).header("User-Agent", UA).timeout(Duration::from_millis(timeout_ms)).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    serde_json::from_str(&resp.text().await.ok()?).ok()
}

fn squash(v: &str) -> String {
    NON_ALNUM.replace_all(&v.to_lowercase(), "").into_owned()
}

pub async fn lookup(ctx: &SharedCtx, name: &str) -> Option<ArtistBio> {
    let name = name.trim();
    let (wiki, mb) = tokio::join!(wiki_artist_page(ctx, name), mb_find_artist(ctx, name));
    if let Some(page) = wiki {
        return Some(page_bio(&page));
    }
    let (id, started) = mb?;
    if let Some(wait) = Duration::from_millis(1100).checked_sub(started.elapsed()) {
        tokio::time::sleep(wait).await;
    }
    let artist = get_json(ctx, &format!("https://musicbrainz.org/ws/2/artist/{id}?inc=url-rels+genres+tags&fmt=json"), 5000).await?;
    if let Some(title) = mb_wikipedia_title(ctx, &artist).await {
        if let Some(Some(page)) = wiki_pages(ctx, &[title]).await.into_iter().next() {
            return Some(page_bio(&page));
        }
    }
    let text = compose_from_musicbrainz(&artist);
    (!text.is_empty()).then(|| ArtistBio { text, image: String::new() })
}

fn page_bio(page: &Value) -> ArtistBio {
    ArtistBio {
        text: clean_extract(&s(page, "extract")),
        image: page.pointer("/original/source").map(js_string).unwrap_or_default(),
    }
}

async fn wiki_pages(ctx: &SharedCtx, titles: &[String]) -> Vec<Option<Value>> {
    let url = format!(
        "https://en.wikipedia.org/w/api.php?action=query&format=json&formatversion=2&redirects=1\
         &prop=extracts%7Cdescription%7Cpageprops%7Cpageimages&exintro=1&explaintext=1&exsentences=5&exlimit=max\
         &piprop=original&ppprop=disambiguation&titles={}",
        enc(&titles.join("|"))
    );
    let Some(data) = get_json(ctx, &url, 4000).await else { return vec![None; titles.len()] };
    let renamed = |key: &str, title: &str| {
        data.pointer(&format!("/query/{key}"))
            .and_then(Value::as_array)
            .and_then(|list| list.iter().find(|m| s(m, "from") == title))
            .map(|m| s(m, "to"))
    };
    let pages = data.pointer("/query/pages").and_then(Value::as_array).cloned().unwrap_or_default();
    titles
        .iter()
        .map(|t| {
            let t = renamed("normalized", t).unwrap_or_else(|| t.clone());
            let t = renamed("redirects", &t).unwrap_or(t);
            pages
                .iter()
                .find(|p| s(p, "title") == t)
                .filter(|p| p.get("missing").is_none() && p.pointer("/pageprops/disambiguation").is_none() && !s(p, "extract").trim().is_empty())
                .cloned()
        })
        .collect()
}

async fn wiki_artist_page(ctx: &SharedCtx, name: &str) -> Option<Value> {
    let titles: Vec<String> = ["band", "musician", "singer", "rapper", "group", "duo", "DJ"]
        .iter()
        .map(|k| format!("{name} ({k})"))
        .chain([name.to_string()])
        .collect();
    wiki_pages(ctx, &titles).await.into_iter().flatten().find(is_music_article)
}

fn clean_extract(text: &str) -> String {
    PRONUNCIATION.replace_all(text, "").replace(" ,", ",").trim().to_string()
}

fn is_music_article(page: &Value) -> bool {
    let description = s(page, "description");
    let first_sentence: String = s(page, "extract").chars().take(240).collect();
    let first_sentence = first_sentence.split(". ").next().unwrap_or_default();
    if NOT_AN_ARTIST.is_match(&description) || IS_A_GENRE.is_match(first_sentence) {
        return false;
    }
    MUSIC_SUBJECT.is_match(&description) || MUSIC_SUBJECT.is_match(first_sentence)
}

async fn mb_find_artist(ctx: &SharedCtx, name: &str) -> Option<(String, Instant)> {
    let started = Instant::now();
    let query = format!("artist:\"{}\"", name.replace('"', ""));
    let data = get_json(ctx, &format!("https://musicbrainz.org/ws/2/artist/?query={}&fmt=json&limit=5", enc(&query)), 5000).await?;
    let wanted = squash(name);
    let artists = arr(&data, "artists");
    let found = artists
        .iter()
        .filter(|a| num(a, "score") >= 90.0)
        .find(|a| squash(&s(a, "name")) == wanted || arr(a, "aliases").iter().any(|al| squash(&s(al, "name")) == wanted))?;
    Some((s(found, "id"), started))
}

async fn mb_wikipedia_title(ctx: &SharedCtx, artist: &Value) -> Option<String> {
    let url_of = |kind: &str| {
        arr(artist, "relations")
            .iter()
            .find(|r| s(r, "type") == kind)
            .and_then(|r| r.pointer("/url/resource"))
            .map(js_string)
    };
    if let Some(link) = url_of("wikipedia") {
        if let Some(title) = link.strip_prefix("https://en.wikipedia.org/wiki/") {
            return urlencoding::decode(title).ok().map(|t| t.replace('_', " "));
        }
    }
    let qid = url_of("wikidata")?.rsplit('/').next()?.to_string();
    let url = format!("https://www.wikidata.org/w/api.php?action=wbgetentities&ids={}&props=sitelinks&sitefilter=enwiki&format=json", enc(&qid));
    let data = get_json(ctx, &url, 4000).await?;
    data.pointer(&format!("/entities/{qid}/sitelinks/enwiki/title")).map(js_string).filter(|t| !t.is_empty())
}

fn article(word: &str) -> &'static str {
    if word.starts_with(|c: char| "aeiouAEIOU".contains(c)) { "an" } else { "a" }
}

fn compose_from_musicbrainz(a: &Value) -> String {
    let name = s(a, "name");
    let kind = s(a, "type");
    let area = a.pointer("/area/name").map(js_string).unwrap_or_default();
    let begin_area = a.pointer("/begin-area/name").map(js_string).unwrap_or_default();
    let year: String = a.pointer("/life-span/begin").map(js_string).unwrap_or_default().chars().take(4).collect();
    let disambiguation = s(a, "disambiguation");
    let mut genres: Vec<String> = arr(a, "genres").iter().map(|g| s(g, "name")).collect();
    if genres.is_empty() {
        let mut tags: Vec<&Value> = arr(a, "tags").iter().filter(|t| num(t, "count") > 0.0).collect();
        tags.sort_by(|x, y| num(y, "count").total_cmp(&num(x, "count")));
        genres = tags.into_iter().map(|t| s(t, "name")).filter(|t| MUSIC_GENRE_LIKE.is_match(t)).collect();
    }
    genres.truncate(4);

    let described = MUSIC_SUBJECT.is_match(&disambiguation);
    if name.is_empty() || (!described && area.is_empty() && year.is_empty() && genres.is_empty()) {
        return String::new();
    }
    let what = if described {
        disambiguation.clone()
    } else {
        match kind.as_str() {
            "Group" => "music group".to_string(),
            "Orchestra" => "orchestra".to_string(),
            "Choir" => "choir".to_string(),
            _ => "music artist".to_string(),
        }
    };
    let mut out = format!("{name} is {} {what}", article(&what));
    if !described && !area.is_empty() {
        out.push_str(&format!(" from {area}"));
    }
    out.push('.');
    if !year.is_empty() {
        let since = match kind.as_str() {
            "Person" => format!(" Born in {year}"),
            "Group" | "Orchestra" | "Choir" => format!(" Formed in {year}"),
            _ => format!(" Active since {year}"),
        };
        out.push_str(&since);
        if !begin_area.is_empty() && begin_area != area {
            out.push_str(&format!(" in {begin_area}"));
        }
        out.push('.');
    }
    if !genres.is_empty() {
        out.push_str(&format!(" Genres: {}.", genres.join(", ")));
    }
    out
}

static MUSIC_GENRE_LIKE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)rock|pop|hip.?hop|rap|trap|phonk|funk|jazz|blues|soul|r&b|metal|punk|indie|folk|country|electronic|house|techno|edm|dance|ambient|reggae|latin|reggaeton|classical|drill|grime|lo-?fi|wave|core|step|gospel|disco|ska|emo|grunge|garage|bass|beat").unwrap()
});

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_only_music_articles() {
        let page = |d: &str, e: &str| json!({ "description": d, "extract": e });
        assert!(is_music_article(&page("Australian indie rock band", "Vacations is an Australian indie rock band formed in 2015.")));
        assert!(is_music_article(&page("Puerto Rican rapper (born 1994)", "")));
        assert!(!is_music_article(&page("Leisure travel away from home or work", "A vacation or holiday is either a leave of absence from a regular job.")));
        assert!(!is_music_article(&page("Topics referred to by the same term", "Queen most commonly refers to: Queen regnant")));
        assert!(!is_music_article(&page("Subgenre of regional Mexican music", "Corridos tumbados, also known as trap corridos, is a subgenre of regional Mexican music.")));
        assert!(!is_music_article(&page("", "Corridos tumbados is a subgenre of regional Mexican music.")));
        assert!(!is_music_article(&page("2019 single by Bad Bunny", "Callaita is a song by Puerto Rican rapper Bad Bunny.")));
        assert!(is_music_article(&page("English singer (born 1966)", "Rick Astley is an English singer, known for his 1987 song.")));
    }

    #[test]
    fn strips_pronunciation_leftovers() {
        assert_eq!(
            clean_extract("Tame Impala ( TAYM im-PAHL-ə) is the psychedelic music project of Kevin Parker."),
            "Tame Impala is the psychedelic music project of Kevin Parker."
        );
        assert_eq!(
            clean_extract("Beatrice Laus (born June 3, 2000), known as Beabadoobee (; bee-bə-DOO-bee; stylized in all lowercase), is a singer."),
            "Beatrice Laus (born June 3, 2000), known as Beabadoobee, is a singer."
        );
    }

    #[test]
    fn writes_a_summary_from_musicbrainz() {
        let band = json!({
            "name": "Vacations", "type": "Group", "disambiguation": "Australian indie surf rock band",
            "area": { "name": "Australia" }, "begin-area": { "name": "Newcastle" }, "life-span": { "begin": "2015" },
            "genres": [{ "name": "indie rock" }, { "name": "surf rock" }]
        });
        assert_eq!(
            compose_from_musicbrainz(&band),
            "Vacations is an Australian indie surf rock band. Formed in 2015 in Newcastle. Genres: indie rock, surf rock."
        );
        let rapper = json!({ "name": "El Bogueto", "type": "Person", "disambiguation": "Mexican rapper", "area": { "name": "Mexico" } });
        assert_eq!(compose_from_musicbrainz(&rapper), "El Bogueto is a Mexican rapper.");
        let person = json!({ "name": "MXSTERIXD", "type": "Person", "area": { "name": "Brazil" }, "tags": [{ "name": "phonk", "count": 2 }, { "name": "seen live", "count": 5 }] });
        assert_eq!(compose_from_musicbrainz(&person), "MXSTERIXD is a music artist from Brazil. Genres: phonk.");
        assert_eq!(compose_from_musicbrainz(&json!({ "name": "Nobody", "type": "Person" })), "");
    }
}
