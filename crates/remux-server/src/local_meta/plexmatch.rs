//! Plex `.plexmatch` hint files: one `key: value` hint per line, `#` starts a
//! comment, and keys are case-insensitive. A file in the series folder names
//! the show and can map files to episodes; one in a season folder sets that
//! folder's season and maps its own files.
//!
//! `pattern` hints are not read.

use regex::Regex;

use super::{SidecarIds, sidecar_text};

/// An `episode:` hint: the file at `path` (relative to the `.plexmatch`
/// folder) is this episode. `season` is `None` for an `E02`/`02` hint, which
/// takes the season from the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeHint {
    pub season: Option<i64>,
    pub episode: i64,
    pub episode_end: Option<i64>,
    pub path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlexMatch {
    pub title: Option<String>,
    pub year: Option<i64>,
    pub season: Option<i64>,
    pub ids: SidecarIds,
    pub episodes: Vec<EpisodeHint>,
}

impl PlexMatch {
    /// The episode hint for `rel_path`, the file's path relative to this
    /// `.plexmatch` folder. Paths compare case-insensitively, ignoring a
    /// leading `/`.
    pub fn episode_for(&self, rel_path: &str) -> Option<&EpisodeHint> {
        let rel_path = rel_path.trim_start_matches('/');
        self.episodes
            .iter()
            .find(|hint| {
                hint.path
                    .trim_start_matches('/')
                    .eq_ignore_ascii_case(rel_path)
            })
    }
}

pub fn parse(bytes: &[u8]) -> PlexMatch {
    let mut out = PlexMatch::default();
    for line in sidecar_text(bytes).lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "title" | "show" => {
                if out
                    .title
                    .is_none()
                    && !value.is_empty()
                {
                    out.title = Some(value.to_string());
                }
            }
            "year" => {
                out.year = out
                    .year
                    .or_else(|| {
                        value
                            .parse()
                            .ok()
                    })
            }
            "season" => {
                out.season = out
                    .season
                    .or_else(|| {
                        value
                            .parse()
                            .ok()
                    })
            }
            "tvdbid" => out
                .ids
                .set("tvdb", value),
            "tmdbid" => out
                .ids
                .set("tmdb", value),
            "imdbid" => out
                .ids
                .set("imdb", value),
            "guid" => {
                if let Some((source, id)) = value.split_once("://") {
                    out.ids
                        .set(source, id);
                }
            }
            "episode" | "ep" => out
                .episodes
                .extend(episode_hint(value)),
            _ => {}
        }
    }
    out
}

/// `S01E01: path`, `S01E01-E02: path`, `SP01: path`, `E02: path` or
/// `02: path`.
fn episode_hint(value: &str) -> Option<EpisodeHint> {
    static RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(
            r"(?i)^(?:s(\d{1,4})e|(sp)|e)?(\d{1,4})(?:-(?:s\d{1,4})?e?(\d{1,4}))?\s*:\s*(.+)$",
        )
        .unwrap()
    });
    let caps = RE.captures(value)?;
    let season = match (caps.get(1), caps.get(2)) {
        (Some(s), _) => Some(
            s.as_str()
                .parse()
                .ok()?,
        ),
        (None, Some(_)) => Some(0),
        (None, None) => None,
    };
    Some(EpisodeHint {
        season,
        episode: caps[3]
            .parse()
            .ok()?,
        episode_end: caps
            .get(4)
            .and_then(|m| {
                m.as_str()
                    .parse()
                    .ok()
            }),
        path: caps[5]
            .trim()
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sonarr_output() {
        let pm = parse(
            b"Title: Formula 1\nYear: 1950\nTvdbId: 387219\nImdbId: tt0000001\n\
              Episode: S2026E91: Season 2026/Formula 1 - S2026E91 - Race.mkv\n",
        );
        assert_eq!(
            pm.title
                .as_deref(),
            Some("Formula 1")
        );
        assert_eq!(pm.year, Some(1950));
        assert_eq!(
            pm.ids,
            SidecarIds {
                imdb: Some("tt0000001".into()),
                tmdb: None,
                tvdb: Some(387219),
            }
        );
        let hint = pm
            .episode_for("season 2026/formula 1 - s2026e91 - race.mkv")
            .unwrap();
        assert_eq!((hint.season, hint.episode), (Some(2026), 91));
    }

    #[test]
    fn reads_comments_case_and_every_episode_form() {
        let pm = parse(
            b"# a comment\nSHOW: The Show\nguid: tmdb://1399\nimdbid: 0944947\nseason: 2\n\
              ep: S01E01-E02: /Season 01/a.mkv\nep: SP01: special.mkv\nep: E03: c.mkv\nep: 04: d.mkv\n\
              pattern: S{s}E{e}\n",
        );
        assert_eq!(
            pm.title
                .as_deref(),
            Some("The Show")
        );
        assert_eq!(pm.season, Some(2));
        assert_eq!(
            pm.ids
                .tmdb,
            Some(1399)
        );
        assert_eq!(
            pm.ids
                .imdb
                .as_deref(),
            Some("tt0944947")
        );
        let forms: Vec<(Option<i64>, i64, Option<i64>)> = pm
            .episodes
            .iter()
            .map(|h| (h.season, h.episode, h.episode_end))
            .collect();
        assert_eq!(
            forms,
            vec![
                (Some(1), 1, Some(2)),
                (Some(0), 1, None),
                (None, 3, None),
                (None, 4, None),
            ]
        );
        assert!(
            pm.episode_for("Season 01/a.mkv")
                .is_some(),
            "a leading slash in the hint is ignored"
        );
    }
}
