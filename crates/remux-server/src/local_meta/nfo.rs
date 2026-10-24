//! Kodi and Emby/Jellyfin NFO files. Both use the same elements, so one
//! parser serves them. Real-world NFOs are messy, so it tolerates a byte order
//! mark, a missing XML declaration, several root elements (multi-episode
//! files), unknown HTML entities, and a provider URL after the XML or instead
//! of it.

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate};
use quick_xml::{events::Event, reader::Reader};
use regex::Regex;

use super::{LocalMeta, SidecarIds, sidecar_text};
use crate::db::{self, ImageKind};

/// What an NFO's root element describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NfoKind {
    Movie,
    TvShow,
    Season,
    Episode,
    /// Any other root, or a URL-only file.
    Other,
}

/// One root element of an NFO file.
#[derive(Debug, Clone, PartialEq)]
pub struct Nfo {
    pub kind: NfoKind,
    pub meta: LocalMeta,
    /// `<showtitle>`: the series an episode belongs to.
    pub show_title: Option<String>,
    /// `<season>`, or `<seasonnumber>` in a Jellyfin `season.nfo`.
    pub season: Option<i64>,
    pub episode: Option<i64>,
    /// Last episode a multi-episode file covers.
    pub episode_end: Option<i64>,
    /// `<namedseason number="N">` names from a Kodi `tvshow.nfo`.
    pub named_seasons: BTreeMap<i64, String>,
    /// Remote artwork the NFO links to.
    pub art: Vec<NfoArt>,
}

/// A `<thumb>` or `<fanart><thumb>` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NfoArt {
    pub kind: ImageKind,
    pub url: String,
    /// For a season image in a `tvshow.nfo`, the season it shows.
    pub season: Option<i64>,
}

/// Parses an NFO file into its root elements, first to last. A file with no
/// elements but a provider URL gives one `NfoKind::Other` entry with its ids;
/// a URL after the XML adds ids the first element lacks.
pub fn parse(bytes: &[u8]) -> Vec<Nfo> {
    let text = sidecar_text(bytes);
    let (roots, outside) = parse_elements(&text);
    let url_ids = ids_from_urls(&outside);
    let mut docs: Vec<Nfo> = roots
        .iter()
        .map(nfo_from_element)
        .collect();
    match docs.first_mut() {
        Some(first) => {
            first
                .meta
                .ids = std::mem::take(
                &mut first
                    .meta
                    .ids,
            )
            .or(&url_ids);
        }
        None if !url_ids.is_empty() => docs.push(Nfo {
            kind: NfoKind::Other,
            meta: LocalMeta {
                ids: url_ids,
                ..Default::default()
            },
            show_title: None,
            season: None,
            episode: None,
            episode_end: None,
            named_seasons: BTreeMap::new(),
            art: Vec::new(),
        }),
        None => {}
    }
    docs
}

/// The episode a (possibly multi-episode) NFO describes: the first
/// `<episodedetails>` with the titles of all of them joined, and the highest
/// episode number of its season as `episode_end`, as Jellyfin reads them.
pub fn episode(docs: &[Nfo]) -> Option<Nfo> {
    let mut episodes = docs
        .iter()
        .filter(|d| d.kind == NfoKind::Episode);
    let mut first = episodes
        .next()?
        .clone();
    for other in episodes {
        if let Some(title) = other
            .meta
            .title
            .as_deref()
        {
            first
                .meta
                .title = Some(
                match first
                    .meta
                    .title
                {
                    Some(t) => format!("{t} / {title}"),
                    None => title.to_string(),
                },
            );
        }
        if other.season == first.season {
            if let Some(n) = other.episode {
                first.episode_end = Some(
                    first
                        .episode_end
                        .unwrap_or(n)
                        .max(n),
                );
            }
        }
    }
    Some(first)
}

/// The first root element of `kind`, or of any kind when there is only an
/// `NfoKind::Other` entry (a URL-only file).
pub fn first_of(docs: &[Nfo], kind: NfoKind) -> Option<&Nfo> {
    docs.iter()
        .find(|d| d.kind == kind)
        .or_else(|| {
            docs.iter()
                .find(|d| d.kind == NfoKind::Other)
        })
}

#[derive(Debug, Default)]
struct Element {
    name: String,
    attrs: Vec<(String, String)>,
    text: String,
    children: Vec<Element>,
}

impl Element {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn children_named<'a>(
        &'a self,
        name: &'a str,
    ) -> impl Iterator<Item = &'a Element> {
        self.children
            .iter()
            .filter(move |c| c.name == name)
    }

    /// Trimmed text of the first non-empty `name` child.
    fn child_text(&self, name: &str) -> Option<String> {
        self.children_named(name)
            .map(|c| {
                c.text
                    .trim()
            })
            .find(|t| !t.is_empty())
            .map(str::to_string)
    }

    fn child_number(&self, name: &str) -> Option<i64> {
        self.child_text(name)
            .and_then(|t| leading_number(&t))
    }
}

/// The XML entities, plus HTML ones NFO writers emit that XML does not define.
fn html_entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{a0}",
        "ndash" => "–",
        "mdash" => "—",
        "hellip" => "…",
        "lsquo" => "‘",
        "rsquo" => "’",
        "ldquo" => "“",
        "rdquo" => "”",
        "eacute" => "é",
        "egrave" => "è",
        "aacute" => "á",
        "agrave" => "à",
        "ouml" => "ö",
        "uuml" => "ü",
        "auml" => "ä",
        "szlig" => "ß",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        _ => return None,
    })
}

/// The root elements of `text`, and the text outside all of them. Parsing stops
/// at the first malformed construct, keeping what was read so far.
fn parse_elements(text: &str) -> (Vec<Element>, String) {
    let mut reader = Reader::from_str(text);
    let config = reader.config_mut();
    config.trim_text(true);
    config.check_end_names = false;

    let mut roots: Vec<Element> = Vec::new();
    let mut stack: Vec<Element> = Vec::new();
    let mut outside = String::new();

    fn element(e: &quick_xml::events::BytesStart<'_>) -> Element {
        Element {
            name: String::from_utf8_lossy(
                e.name()
                    .as_ref(),
            )
            .to_ascii_lowercase(),
            attrs: e
                .attributes()
                .flatten()
                .map(|a| {
                    (
                        String::from_utf8_lossy(
                            a.key
                                .as_ref(),
                        )
                        .to_ascii_lowercase(),
                        String::from_utf8_lossy(&a.value).into_owned(),
                    )
                })
                .collect(),
            ..Default::default()
        }
    }

    fn close(el: Element, stack: &mut [Element], roots: &mut Vec<Element>) {
        match stack.last_mut() {
            Some(parent) => parent
                .children
                .push(el),
            None => roots.push(el),
        }
    }

    loop {
        let text = match reader.read_event() {
            Ok(Event::Start(e)) => {
                stack.push(element(&e));
                continue;
            }
            Ok(Event::Empty(e)) => {
                close(element(&e), &mut stack, &mut roots);
                continue;
            }
            Ok(Event::End(_)) => {
                if let Some(el) = stack.pop() {
                    close(el, &mut stack, &mut roots);
                }
                continue;
            }
            Ok(Event::Text(e)) => e
                .unescape_with(html_entity)
                .map(|t| t.into_owned())
                .unwrap_or_else(|_| String::from_utf8_lossy(&e).into_owned()),
            Ok(Event::CData(e)) => String::from_utf8_lossy(&e).into_owned(),
            Ok(Event::Eof) | Err(_) => break,
            Ok(_) => continue,
        };
        match stack.last_mut() {
            Some(el) => el
                .text
                .push_str(&text),
            None => {
                outside.push_str(&text);
                outside.push('\n');
            }
        }
    }
    while let Some(el) = stack.pop() {
        close(el, &mut stack, &mut roots);
    }
    (roots, outside)
}

/// IMDb, TMDB and TVDB ids from provider URLs in `text`.
fn ids_from_urls(text: &str) -> SidecarIds {
    static IMDB: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)imdb\.com/(?:[a-z]{2}/)?title/(tt\d+)").unwrap()
    });
    static TMDB: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)themoviedb\.org/(?:movie|tv)/(\d+)").unwrap()
    });
    static TVDB: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(
            r"(?i)thetvdb\.com/(?:\S*?[?&](?:series)?id=|(?:dereferrer/)?series/)(\d+)",
        )
        .unwrap()
    });
    let mut ids = SidecarIds::default();
    for (kind, re) in [("imdb", &IMDB), ("tmdb", &TMDB), ("tvdb", &TVDB)] {
        if let Some(caps) = re.captures(text) {
            ids.set(kind, &caps[1]);
        }
    }
    ids
}

/// The number a text starts with (`"120 min"` → 120).
fn leading_number(text: &str) -> Option<i64> {
    let digits: String = text
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits
        .parse()
        .ok()
}

fn parse_date(text: &str) -> Option<NaiveDate> {
    let date = text
        .trim()
        .get(..10)?;
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

/// A rating without Kodi's `Rated ` prefix or a `US:` country prefix.
fn certification(text: &str) -> Option<String> {
    let text = text.trim();
    let text = text
        .strip_prefix("Rated ")
        .unwrap_or(text);
    let text = text
        .rsplit_once(':')
        .map_or(text, |(_, rating)| rating)
        .trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn status(text: &str) -> Option<db::MediaStatus> {
    match text
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "continuing" | "returning series" | "airing" => {
            Some(db::MediaStatus::Continuing)
        }
        "ended" | "canceled" | "cancelled" => Some(db::MediaStatus::Ended),
        _ => None,
    }
}

/// Every non-empty `name` child, with Kodi's ` / ` lists split.
fn list(el: &Element, name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in el
        .children_named(name)
        .flat_map(|c| {
            c.text
                .split(" / ")
        })
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        if !out
            .iter()
            .any(|o| o.eq_ignore_ascii_case(value))
        {
            out.push(value.to_string());
        }
    }
    out
}

fn nfo_from_element(el: &Element) -> Nfo {
    let kind = match el
        .name
        .as_str()
    {
        "movie" => NfoKind::Movie,
        "tvshow" => NfoKind::TvShow,
        "season" => NfoKind::Season,
        "episodedetails" | "episode" => NfoKind::Episode,
        _ => NfoKind::Other,
    };

    let mut ids = SidecarIds::default();
    for uid in el.children_named("uniqueid") {
        if let Some(kind) = uid.attr("type") {
            ids.set(kind, &uid.text);
        }
    }
    for (tag, kind) in [("imdbid", "imdb"), ("tmdbid", "tmdb"), ("tvdbid", "tvdb")] {
        if let Some(value) = el.child_text(tag) {
            ids.set(kind, &value);
        }
    }
    if let Some(id) = el
        .child_text("id")
        .filter(|id| id.starts_with("tt"))
    {
        ids.set("imdb", &id);
    }

    let premiered = ["premiered", "aired", "releasedate", "formed"]
        .into_iter()
        .find_map(|tag| {
            el.child_text(tag)
                .as_deref()
                .and_then(parse_date)
        })
        .or_else(|| {
            el.child_number("year")
                .and_then(|y| NaiveDate::from_ymd_opt(i32::try_from(y).ok()?, 1, 1))
        })
        .filter(|d| d.year() > 1800);

    let named_seasons = el
        .children_named("namedseason")
        .filter_map(|ns| {
            let number = leading_number(ns.attr("number")?)?;
            let name = ns
                .text
                .trim();
            (!name.is_empty()).then(|| (number, name.to_string()))
        })
        .collect();

    Nfo {
        kind,
        meta: LocalMeta {
            title: el.child_text("title"),
            overview: ["plot", "outline", "review"]
                .into_iter()
                .find_map(|tag| el.child_text(tag)),
            premiered,
            end_date: el
                .child_text("enddate")
                .as_deref()
                .and_then(parse_date),
            runtime_minutes: el
                .child_number("runtime")
                .filter(|m| *m > 0),
            certification: el
                .child_text("mpaa")
                .as_deref()
                .and_then(certification),
            status: el
                .child_text("status")
                .as_deref()
                .and_then(status),
            genres: list(el, "genre"),
            studios: list(el, "studio"),
            tags: list(el, "tag"),
            ids,
            ..Default::default()
        },
        show_title: el.child_text("showtitle"),
        season: el
            .child_number("season")
            .or_else(|| el.child_number("seasonnumber")),
        episode: el.child_number("episode"),
        episode_end: el.child_number("episodenumberend"),
        named_seasons,
        art: nfo_art(el),
    }
}

fn is_http_url(text: &str) -> bool {
    text.starts_with("http://") || text.starts_with("https://")
}

/// Kodi's `<thumb aspect=…>` (`season="N" type="season"` for a season image)
/// and `<fanart>` thumbs, which may be relative to the `<fanart url=…>`.
/// Only remote URLs are kept; Jellyfin's local `<art>` paths name files the
/// artwork lookup finds anyway.
fn nfo_art(el: &Element) -> Vec<NfoArt> {
    let mut out = Vec::new();
    for thumb in el.children_named("thumb") {
        let url = thumb
            .text
            .trim();
        if !is_http_url(url) {
            continue;
        }
        let kind = match thumb.attr("aspect") {
            None | Some("poster") | Some("thumb") => ImageKind::Primary,
            Some("landscape") => ImageKind::Thumb,
            Some("clearlogo") | Some("logo") => ImageKind::Logo,
            Some("fanart") => ImageKind::Backdrop,
            Some(_) => continue,
        };
        let season = match thumb.attr("type") {
            Some("season") => match thumb
                .attr("season")
                .and_then(leading_number)
            {
                Some(n) => Some(n),
                // `season="-1"`: every season, which Remux has no slot for.
                None => continue,
            },
            _ => None,
        };
        out.push(NfoArt {
            kind,
            url: url.to_string(),
            season,
        });
    }
    for fanart in el.children_named("fanart") {
        let base = fanart
            .attr("url")
            .unwrap_or_default();
        for thumb in fanart.children_named("thumb") {
            let text = thumb
                .text
                .trim();
            let url = if is_http_url(text) {
                text.to_string()
            } else if is_http_url(base) && !text.is_empty() {
                format!("{}{text}", base.trim_end_matches('/'))
            } else {
                continue;
            };
            out.push(NfoArt {
                kind: ImageKind::Backdrop,
                url,
                season: None,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Sportarr episode NFO, verbatim apart from the plot.
    const SPORTARR_EPISODE: &[u8] =
        b"\xEF\xBB\xBF<?xml version=\"1.0\" encoding=\"utf-8\"?>
<episodedetails>
  <title>Singapore Grand Prix - Race</title>
  <showtitle>Formula 1</showtitle>
  <season>2026</season>
  <episode>91</episode>
  <aired>2026-10-04</aired>
  <plot>Round 18 of the 2026 season at Marina Bay.</plot>
  <genre>Motorsport</genre>
  <uniqueid type=\"sportarr\" default=\"true\">ev-174552</uniqueid>
</episodedetails>";

    #[test]
    fn parses_the_sportarr_episode_nfo() {
        let docs = parse(SPORTARR_EPISODE);
        assert_eq!(docs.len(), 1);
        let ep = &docs[0];
        assert_eq!(ep.kind, NfoKind::Episode);
        assert_eq!(
            ep.meta
                .title
                .as_deref(),
            Some("Singapore Grand Prix - Race")
        );
        assert_eq!(
            ep.show_title
                .as_deref(),
            Some("Formula 1")
        );
        assert_eq!((ep.season, ep.episode), (Some(2026), Some(91)));
        assert_eq!(
            ep.meta
                .premiered,
            NaiveDate::from_ymd_opt(2026, 10, 4)
        );
        assert_eq!(
            ep.meta
                .overview
                .as_deref(),
            Some("Round 18 of the 2026 season at Marina Bay.")
        );
        assert_eq!(
            ep.meta
                .genres,
            vec!["Motorsport"]
        );
        assert!(
            ep.meta
                .ids
                .is_empty(),
            "a sportarr id is not a provider id"
        );
    }

    #[test]
    fn parses_a_kodi_tvshow_with_named_seasons() {
        let docs = parse(
            br#"<tvshow>
  <title>Show</title>
  <plot></plot>
  <outline>Short outline.</outline>
  <premiered>2011-04-17</premiered>
  <status>Ended</status>
  <mpaa>Rated TV-MA</mpaa>
  <genre>Drama / Fantasy</genre>
  <genre>drama</genre>
  <studio>HBO</studio>
  <uniqueid type="tvdb" default="true">121361</uniqueid>
  <uniqueid type="imdb">tt0944947</uniqueid>
  <namedseason number="1">Winter Is Coming</namedseason>
  <namedseason number="2"></namedseason>
  <actor><name>Someone</name><role>Lord</role></actor>
</tvshow>"#,
        );
        let show = first_of(&docs, NfoKind::TvShow).unwrap();
        assert_eq!(
            show.meta
                .overview
                .as_deref(),
            Some("Short outline."),
            "an empty plot falls back to the outline"
        );
        assert_eq!(
            show.meta
                .status,
            Some(db::MediaStatus::Ended)
        );
        assert_eq!(
            show.meta
                .certification
                .as_deref(),
            Some("TV-MA")
        );
        assert_eq!(
            show.meta
                .genres,
            vec!["Drama", "Fantasy"]
        );
        assert_eq!(
            show.meta
                .studios,
            vec!["HBO"]
        );
        assert_eq!(
            show.meta
                .ids,
            SidecarIds {
                imdb: Some("tt0944947".into()),
                tmdb: None,
                tvdb: Some(121361),
            }
        );
        assert_eq!(
            show.named_seasons,
            BTreeMap::from([(1, "Winter Is Coming".to_string())])
        );
    }

    #[test]
    fn reads_jellyfin_id_elements_and_season_numbers() {
        let movie = parse(
            br#"<?xml version="1.0" encoding="utf-8" standalone="yes"?>
<movie>
  <title>Home Movie</title>
  <lockdata>true</lockdata>
  <imdbid>tt0111161</imdbid>
  <tmdbid>278</tmdbid>
  <year>1994</year>
  <runtime>142</runtime>
</movie>"#,
        );
        let movie = first_of(&movie, NfoKind::Movie).unwrap();
        assert_eq!(
            movie
                .meta
                .ids,
            SidecarIds {
                imdb: Some("tt0111161".into()),
                tmdb: Some(278),
                tvdb: None,
            }
        );
        assert_eq!(
            movie
                .meta
                .premiered,
            NaiveDate::from_ymd_opt(1994, 1, 1)
        );
        assert_eq!(
            movie
                .meta
                .runtime_minutes,
            Some(142)
        );

        let season = parse(b"<season><title>Pilot Season</title><seasonnumber>1</seasonnumber></season>");
        let season = first_of(&season, NfoKind::Season).unwrap();
        assert_eq!(season.season, Some(1));
        assert_eq!(
            season
                .meta
                .title
                .as_deref(),
            Some("Pilot Season")
        );
    }

    #[test]
    fn takes_ids_from_a_url_after_the_xml_or_instead_of_it() {
        let combined = parse(
            b"\xEF\xBB\xBF<tvshow><title>Show</title></tvshow>\nhttps://www.thetvdb.com/?tab=series&id=81189\n",
        );
        assert_eq!(
            first_of(&combined, NfoKind::TvShow)
                .unwrap()
                .meta
                .ids
                .tvdb,
            Some(81189)
        );

        let url_only = parse(b"https://www.imdb.com/title/tt0111161/\n");
        assert_eq!(url_only.len(), 1);
        let only = first_of(&url_only, NfoKind::Movie).unwrap();
        assert_eq!(only.kind, NfoKind::Other);
        assert_eq!(
            only.meta
                .ids
                .imdb
                .as_deref(),
            Some("tt0111161")
        );

        assert_eq!(
            ids_from_urls("https://www.themoviedb.org/tv/1399-game-of-thrones").tmdb,
            Some(1399)
        );
        assert!(parse(b"not an nfo").is_empty());
    }

    #[test]
    fn reads_remote_artwork_links() {
        let docs = parse(
            br#"<tvshow>
  <thumb aspect="poster">https://img.example/poster.jpg</thumb>
  <thumb aspect="banner">https://img.example/banner.jpg</thumb>
  <thumb aspect="poster" type="season" season="2">https://img.example/s2.jpg</thumb>
  <thumb aspect="poster" type="season" season="-1">https://img.example/all.jpg</thumb>
  <thumb>/local/poster.jpg</thumb>
  <fanart url="https://img.example/t/p/original"><thumb preview="/w780/f.jpg">/f.jpg</thumb></fanart>
</tvshow>"#,
        );
        let art: Vec<(ImageKind, &str, Option<i64>)> = docs[0]
            .art
            .iter()
            .map(|a| {
                (
                    a.kind,
                    a.url
                        .as_str(),
                    a.season,
                )
            })
            .collect();
        assert_eq!(
            art,
            vec![
                (ImageKind::Primary, "https://img.example/poster.jpg", None),
                (ImageKind::Primary, "https://img.example/s2.jpg", Some(2)),
                (
                    ImageKind::Backdrop,
                    "https://img.example/t/p/original/f.jpg",
                    None
                ),
            ]
        );
    }

    #[test]
    fn joins_a_multi_episode_file() {
        let docs = parse(
            br#"<episodedetails><title>Part One</title><season>1</season><episode>1</episode></episodedetails>
<episodedetails><title>Part Two</title><season>1</season><episode>2</episode></episodedetails>"#,
        );
        let ep = episode(&docs).unwrap();
        assert_eq!(
            ep.meta
                .title
                .as_deref(),
            Some("Part One / Part Two")
        );
        assert_eq!(
            (ep.season, ep.episode, ep.episode_end),
            (Some(1), Some(1), Some(2))
        );
    }

    #[test]
    fn tolerates_html_entities_and_unclosed_elements() {
        let docs = parse(
            b"<movie><title>Caf&eacute;&nbsp;Society</title><plot>Fish &amp; chips",
        );
        let movie = first_of(&docs, NfoKind::Movie).unwrap();
        assert_eq!(
            movie
                .meta
                .title
                .as_deref(),
            Some("Café\u{a0}Society")
        );
        assert_eq!(
            movie
                .meta
                .overview
                .as_deref(),
            Some("Fish & chips")
        );
    }
}
