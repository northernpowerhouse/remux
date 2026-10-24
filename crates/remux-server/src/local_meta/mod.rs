//! Metadata for media files that have no TMDB/TVDB/IMDb match, read from the
//! sidecar files next to them: Kodi and Emby/Jellyfin NFOs, and Plex
//! `.plexmatch` hints.

pub mod nfo;
pub mod plexmatch;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{common, db};

/// Which sidecar naming rules a Local/WebDAV addon reads. One per addon,
/// because the formats name the same files differently.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    strum_macros::Display,
    strum_macros::EnumString,
)]
#[strum(serialize_all = "snake_case")]
pub enum SidecarFormat {
    /// `tvshow.nfo`, `<file>.nfo`, `movie.nfo`; season names from
    /// `<namedseason>` in `tvshow.nfo`.
    #[default]
    Kodi,
    /// As Kodi, plus `season.nfo` in the season folder.
    Jellyfin,
    /// `.plexmatch` in the series and season folders.
    Plex,
    /// Read no sidecars.
    None,
}

impl SidecarFormat {
    pub fn from_cfg(cfg: &serde_json::Value) -> Self {
        cfg["sidecar_format"]
            .as_str()
            .and_then(|s| {
                s.parse()
                    .ok()
            })
            .unwrap_or_default()
    }

    pub fn reads_nfo(self) -> bool {
        matches!(self, Self::Kodi | Self::Jellyfin)
    }
}

/// Provider ids a sidecar names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarIds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imdb: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmdb: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tvdb: Option<i64>,
}

impl SidecarIds {
    pub fn is_empty(&self) -> bool {
        self.imdb
            .is_none()
            && self
                .tmdb
                .is_none()
            && self
                .tvdb
                .is_none()
    }

    /// Fills the ids this one lacks from `other`.
    pub fn or(mut self, other: &Self) -> Self {
        self.imdb = self
            .imdb
            .or_else(|| {
                other
                    .imdb
                    .clone()
            });
        self.tmdb = self
            .tmdb
            .or(other.tmdb);
        self.tvdb = self
            .tvdb
            .or(other.tvdb);
        self
    }

    pub fn to_external_ids(&self) -> db::ExternalIds {
        db::ExternalIds {
            imdb: self
                .imdb
                .clone()
                .and_then(|id| db::NonEmptyString::try_new(id).ok()),
            tmdb: self.tmdb,
            tvdb: self.tvdb,
            ..Default::default()
        }
    }

    /// An IMDb id (`tt` + digits), with the `tt` added when only digits are
    /// given.
    fn imdb_from(raw: &str) -> Option<String> {
        let raw = raw.trim();
        let digits = raw
            .strip_prefix("tt")
            .unwrap_or(raw);
        (!digits.is_empty()
            && digits
                .bytes()
                .all(|b| b.is_ascii_digit()))
        .then(|| format!("tt{digits}"))
    }

    fn number_from(raw: &str) -> Option<i64> {
        raw.trim()
            .parse()
            .ok()
            .filter(|n| *n > 0)
    }

    /// Records `value` under the provider `kind` names (`imdb`, `tmdb`,
    /// `tvdb`, case-insensitive). Other providers are ignored.
    pub fn set(&mut self, kind: &str, value: &str) {
        match kind
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "imdb" => {
                self.imdb = self
                    .imdb
                    .take()
                    .or_else(|| Self::imdb_from(value))
            }
            "tmdb" | "themoviedb" => {
                self.tmdb = self
                    .tmdb
                    .or_else(|| Self::number_from(value))
            }
            "tvdb" | "thetvdb" => {
                self.tvdb = self
                    .tvdb
                    .or_else(|| Self::number_from(value))
            }
            _ => {}
        }
    }
}

/// What the sidecars say about one indexed file, stored as JSON in
/// `opendal_files.local_meta`. For an episode, `series` and `season_title`
/// carry the show- and season-level data, so every episode row holds what its
/// series and season need.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LocalMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub premiered: Option<NaiveDate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_date: Option<NaiveDate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_minutes: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certification: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<db::MediaStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub studios: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "SidecarIds::is_empty")]
    pub ids: SidecarIds,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<Box<LocalMeta>>,
}

impl LocalMeta {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// A metadata patch for `media` (its id and kind are kept), as a
    /// `MetaAddon` returns it.
    pub fn patch_for(&self, media: &db::Media) -> db::Media {
        let mut patch = db::Media {
            id: media.id,
            kind: media
                .kind
                .clone(),
            ..Default::default()
        };
        self.apply_to(&mut patch);
        patch
    }

    /// Writes these fields onto `media`, replacing what they cover.
    pub fn apply_to(&self, media: &mut db::Media) {
        if let Some(title) = self
            .title
            .as_ref()
            .filter(|t| !t.is_empty())
        {
            media.title = title.clone();
        }
        if self
            .overview
            .is_some()
        {
            media.description = self
                .overview
                .clone();
        }
        if let Some(date) = self.premiered {
            media.released_at = date.and_hms_opt(0, 0, 0);
        }
        if let Some(date) = self.end_date {
            media.end_date = date.and_hms_opt(0, 0, 0);
        }
        if let Some(minutes) = self.runtime_minutes {
            media.runtime = Some(minutes * 60);
        }
        if self
            .certification
            .is_some()
        {
            media.certification = self
                .certification
                .clone();
        }
        if self
            .status
            .is_some()
        {
            media.status = self
                .status
                .clone();
        }
        if !self
            .tags
            .is_empty()
        {
            media.tags = self
                .tags
                .clone();
        }
        let relations: Vec<(db::MediaRelation, db::Media)> = self
            .genres
            .iter()
            .map(|name| (db::MediaKind::Genre, name))
            .chain(
                self.studios
                    .iter()
                    .map(|name| (db::MediaKind::Studio, name)),
            )
            .map(|(kind, name)| name_relation(media.id, kind, name))
            .collect();
        if !relations.is_empty() {
            media.relations = Some(relations);
        }
    }
}

/// A genre or studio relation, keyed by name the way the TMDB addon keys them
/// so the same genre from either source is one row.
fn name_relation(
    left_media_id: Uuid,
    kind: db::MediaKind,
    name: &str,
) -> (db::MediaRelation, db::Media) {
    let id = common::stable_media_uuid(&kind, &name.to_lowercase());
    (
        db::MediaRelation {
            left_media_id,
            right_media_id: id,
            ..Default::default()
        },
        db::Media {
            id,
            title: name.to_string(),
            kind,
            ..Default::default()
        },
    )
}

/// The UTF-8 text of a sidecar, without a byte order mark.
pub(crate) fn sidecar_text(bytes: &[u8]) -> String {
    let bytes = bytes
        .strip_prefix(b"\xEF\xBB\xBF")
        .unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_ids_normalise_values() {
        let mut ids = SidecarIds::default();
        ids.set("IMDB", "0944947");
        ids.set("tmdb", "1399");
        ids.set("tvdb", "0");
        ids.set("sportarr", "ev-174552");
        assert_eq!(
            ids,
            SidecarIds {
                imdb: Some("tt0944947".into()),
                tmdb: Some(1399),
                tvdb: None,
            }
        );
        ids.set("imdb", "tt1111111");
        assert_eq!(
            ids.imdb
                .as_deref(),
            Some("tt0944947"),
            "the first id of a kind wins"
        );
    }

    #[test]
    fn patch_carries_text_dates_and_relations() {
        let meta = LocalMeta {
            title: Some("Singapore Grand Prix - Race".into()),
            overview: Some("Round 18.".into()),
            premiered: NaiveDate::from_ymd_opt(2026, 10, 4),
            runtime_minutes: Some(120),
            genres: vec!["Motorsport".into()],
            studios: vec!["F1 TV".into()],
            ..Default::default()
        };
        let media = db::Media {
            kind: db::MediaKind::Episode,
            ..Default::default()
        };
        let patch = meta.patch_for(&media);
        assert_eq!(patch.id, media.id);
        assert_eq!(patch.kind, db::MediaKind::Episode);
        assert_eq!(patch.title, "Singapore Grand Prix - Race");
        assert_eq!(
            patch
                .description
                .as_deref(),
            Some("Round 18.")
        );
        assert_eq!(
            patch.released_at,
            NaiveDate::from_ymd_opt(2026, 10, 4).and_then(|d| d.and_hms_opt(0, 0, 0))
        );
        assert_eq!(patch.runtime, Some(7200));
        let rels = patch
            .relations
            .unwrap();
        assert_eq!(rels.len(), 2);
        assert_eq!(
            rels[0]
                .1
                .kind,
            db::MediaKind::Genre
        );
        assert_eq!(
            rels[1]
                .1
                .kind,
            db::MediaKind::Studio
        );
        assert!(
            rels.iter()
                .all(|(r, _)| r.left_media_id == media.id)
        );
    }

    #[test]
    fn local_meta_json_omits_empty_fields() {
        let meta = LocalMeta {
            title: Some("A".into()),
            ..Default::default()
        };
        assert_eq!(serde_json::to_string(&meta).unwrap(), r#"{"title":"A"}"#);
        assert!(LocalMeta::default().is_empty());
    }
}
