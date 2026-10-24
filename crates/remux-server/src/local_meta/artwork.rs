//! Which image files next to a movie or show are its artwork, per sidecar
//! format. Names compare case-insensitively; the first candidate found for an
//! image kind wins.
//!
//! Remux has no slot for banners, disc art, clear art or theme music, so those
//! names are not looked for.

use crate::db::ImageKind;

use super::SidecarFormat;

/// Picture extensions read, in the order they are tried.
pub const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "tbn"];

pub fn is_image_ext(ext: &str) -> bool {
    IMAGE_EXTENSIONS.contains(&ext)
}

/// What the artwork belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtLevel {
    Movie,
    Series,
    Season(i64),
    Episode,
}

/// Folders a level's artwork can live in, operator-relative.
#[derive(Debug, Clone, Copy)]
pub struct ArtDirs<'a> {
    /// The movie's or episode's own folder.
    pub file_dir: &'a str,
    pub series_dir: Option<&'a str>,
    /// The season folder, when episodes sit in one below the series folder.
    pub season_dir: Option<&'a str>,
}

/// One place an image may be: `{dir}/{base}.{ext}` for any image extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtCandidate {
    pub kind: ImageKind,
    pub dir: String,
    pub base: String,
}

/// Where `format` looks for the artwork of `level`, best first. `stem` is the
/// video file name without its extension (unused for series and seasons).
pub fn candidates(
    format: SidecarFormat,
    level: ArtLevel,
    stem: &str,
    dirs: ArtDirs<'_>,
) -> Vec<ArtCandidate> {
    use ImageKind::*;

    let mut out = Vec::new();
    let mut add = |kind: ImageKind, dir: Option<&str>, bases: &[String]| {
        if let Some(dir) = dir {
            out.extend(
                bases
                    .iter()
                    .map(|base| ArtCandidate {
                        kind,
                        dir: dir.to_string(),
                        base: base.clone(),
                    }),
            );
        }
    };
    let names = |names: &[&str]| -> Vec<String> {
        names
            .iter()
            .map(|n| n.to_string())
            .collect()
    };
    // `<stem>-{suffix}` for each suffix.
    let suffixed = |suffixes: &[&str]| -> Vec<String> {
        suffixes
            .iter()
            .map(|s| format!("{stem}-{s}"))
            .collect()
    };
    let file_dir = Some(dirs.file_dir);

    match (format, level) {
        (SidecarFormat::None, _) => {}

        (SidecarFormat::Kodi, ArtLevel::Movie) => {
            add(Primary, file_dir, &suffixed(&["poster"]));
            add(Primary, file_dir, &names(&["poster", "folder"]));
            add(Backdrop, file_dir, &suffixed(&["fanart"]));
            add(Backdrop, file_dir, &names(&["fanart"]));
            add(Logo, file_dir, &suffixed(&["clearlogo"]));
            add(Logo, file_dir, &names(&["clearlogo"]));
            add(Thumb, file_dir, &suffixed(&["landscape"]));
            add(Thumb, file_dir, &names(&["landscape"]));
        }
        (SidecarFormat::Jellyfin, ArtLevel::Movie) => {
            const PRIMARY: &[&str] = &["poster", "folder", "cover", "default", "movie"];
            const BACKDROP: &[&str] = &["fanart", "backdrop", "background", "art"];
            const LOGO: &[&str] = &["logo", "clearlogo"];
            const THUMB: &[&str] = &["landscape", "thumb"];
            add(Primary, file_dir, &suffixed(PRIMARY));
            add(Primary, file_dir, &names(PRIMARY));
            add(Backdrop, file_dir, &suffixed(BACKDROP));
            add(Backdrop, file_dir, &names(BACKDROP));
            add(Logo, file_dir, &suffixed(LOGO));
            add(Logo, file_dir, &names(LOGO));
            add(Thumb, file_dir, &suffixed(THUMB));
            add(Thumb, file_dir, &names(THUMB));
        }
        (SidecarFormat::Plex, ArtLevel::Movie) => {
            add(Primary, file_dir, &[stem.to_string()]);
            add(
                Primary,
                file_dir,
                &names(&["poster", "cover", "default", "folder", "movie"]),
            );
            add(Backdrop, file_dir, &suffixed(&["fanart"]));
            add(
                Backdrop,
                file_dir,
                &names(&["fanart", "art", "backdrop", "background"]),
            );
            add(Logo, file_dir, &suffixed(&["clearlogo"]));
            add(Logo, file_dir, &names(&["clearlogo", "logo"]));
        }

        (SidecarFormat::Kodi, ArtLevel::Series) => {
            add(Primary, dirs.series_dir, &names(&["poster"]));
            add(Backdrop, dirs.series_dir, &names(&["fanart"]));
            add(Logo, dirs.series_dir, &names(&["clearlogo"]));
            add(Thumb, dirs.series_dir, &names(&["landscape"]));
        }
        (SidecarFormat::Jellyfin, ArtLevel::Series) => {
            add(
                Primary,
                dirs.series_dir,
                &names(&["poster", "folder", "cover", "default", "show"]),
            );
            add(
                Backdrop,
                dirs.series_dir,
                &names(&["fanart", "backdrop", "background", "art"]),
            );
            add(Logo, dirs.series_dir, &names(&["logo", "clearlogo"]));
            add(Thumb, dirs.series_dir, &names(&["landscape", "thumb"]));
        }
        (SidecarFormat::Plex, ArtLevel::Series) => {
            add(
                Primary,
                dirs.series_dir,
                &names(&["poster", "folder", "show"]),
            );
            add(
                Backdrop,
                dirs.series_dir,
                &names(&["fanart", "art", "backdrop", "background"]),
            );
            add(Logo, dirs.series_dir, &names(&["clearlogo", "logo"]));
        }

        (SidecarFormat::Kodi | SidecarFormat::Jellyfin, ArtLevel::Season(n)) => {
            // `seasonNN-*` in the show folder.
            let season_names = |suffix: &str| -> Vec<String> {
                let mut bases: Vec<String> = season_numbers(n)
                    .into_iter()
                    .map(|nn| format!("season{nn}-{suffix}"))
                    .collect();
                if n == 0 {
                    bases.push(format!("season-specials-{suffix}"));
                }
                if format == SidecarFormat::Kodi {
                    bases.push(format!("season-all-{suffix}"));
                }
                bases
            };
            add(Primary, dirs.series_dir, &season_names("poster"));
            add(Backdrop, dirs.series_dir, &season_names("fanart"));
            add(Thumb, dirs.series_dir, &season_names("landscape"));
            if format == SidecarFormat::Jellyfin {
                // Jellyfin also reads plain names in the season folder.
                add(
                    Primary,
                    dirs.season_dir,
                    &names(&["poster", "folder", "cover", "default"]),
                );
                add(
                    Backdrop,
                    dirs.season_dir,
                    &names(&["fanart", "backdrop", "background"]),
                );
            }
        }
        (SidecarFormat::Plex, ArtLevel::Season(n)) => {
            let bases: Vec<String> = season_numbers(n)
                .into_iter()
                .map(|nn| format!("season{nn}"))
                .collect();
            add(Primary, dirs.season_dir, &bases);
            if n == 0 {
                add(
                    Primary,
                    dirs.series_dir,
                    &names(&["season-specials-poster"]),
                );
            }
        }

        (SidecarFormat::Kodi, ArtLevel::Episode) => {
            add(Primary, file_dir, &suffixed(&["thumb"]));
        }
        (SidecarFormat::Jellyfin, ArtLevel::Episode) => {
            add(Primary, file_dir, &suffixed(&["thumb"]));
            add(Primary, file_dir, &[stem.to_string()]);
            let metadata = if dirs
                .file_dir
                .is_empty()
            {
                "metadata".to_string()
            } else {
                format!("{}/metadata", dirs.file_dir)
            };
            add(Primary, Some(&metadata), &[stem.to_string()]);
        }
        (SidecarFormat::Plex, ArtLevel::Episode) => {
            add(Primary, file_dir, &[stem.to_string()]);
        }
    }
    out
}

/// Ways a season number is written in artwork names: two-digit padded first,
/// then unpadded (`season2026-poster` for a year season).
fn season_numbers(n: i64) -> Vec<String> {
    let padded = format!("{n:02}");
    let plain = n.to_string();
    if padded == plain {
        vec![padded]
    } else {
        vec![padded, plain]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(
        format: SidecarFormat,
        level: ArtLevel,
        stem: &str,
        dirs: ArtDirs<'_>,
        files: &[&str],
    ) -> Vec<(ImageKind, String)> {
        let mut out: Vec<(ImageKind, String)> = Vec::new();
        for c in candidates(format, level, stem, dirs) {
            if out
                .iter()
                .any(|(k, _)| *k == c.kind)
            {
                continue;
            }
            let hit = IMAGE_EXTENSIONS
                .iter()
                .map(|ext| {
                    let name = format!("{}.{ext}", c.base);
                    if c.dir
                        .is_empty()
                    {
                        name
                    } else {
                        format!("{}/{name}", c.dir)
                    }
                })
                .find_map(|path| {
                    files
                        .iter()
                        .find(|f| f.eq_ignore_ascii_case(&path))
                        .map(|f| f.to_string())
                });
            if let Some(hit) = hit {
                out.push((c.kind, hit));
            }
        }
        out
    }

    const SHOW: ArtDirs<'static> = ArtDirs {
        file_dir: "Show/Season 01",
        series_dir: Some("Show"),
        season_dir: Some("Show/Season 01"),
    };

    #[test]
    fn kodi_movie_prefers_the_file_specific_poster() {
        let dirs = ArtDirs {
            file_dir: "Films",
            series_dir: None,
            season_dir: None,
        };
        let art = found(
            SidecarFormat::Kodi,
            ArtLevel::Movie,
            "Film (2020)",
            dirs,
            &[
                "Films/poster.jpg",
                "Films/Film (2020)-poster.PNG",
                "Films/fanart.jpg",
            ],
        );
        assert_eq!(
            art,
            vec![
                (
                    ImageKind::Primary,
                    "Films/Film (2020)-poster.PNG".to_string()
                ),
                (ImageKind::Backdrop, "Films/fanart.jpg".to_string()),
            ]
        );
    }

    #[test]
    fn season_posters_follow_each_format() {
        let files = [
            "Show/season01-poster.jpg",
            "Show/Season 01/Season01.jpg",
            "Show/Season 01/folder.jpg",
            "Show/season-specials-poster.jpg",
            "Show/season2026-fanart.jpg",
        ];
        assert_eq!(
            found(SidecarFormat::Kodi, ArtLevel::Season(1), "", SHOW, &files),
            vec![(ImageKind::Primary, "Show/season01-poster.jpg".to_string())]
        );
        assert_eq!(
            found(SidecarFormat::Plex, ArtLevel::Season(1), "", SHOW, &files),
            vec![(
                ImageKind::Primary,
                "Show/Season 01/Season01.jpg".to_string()
            )]
        );
        assert_eq!(
            found(
                SidecarFormat::Jellyfin,
                ArtLevel::Season(0),
                "",
                SHOW,
                &files
            ),
            vec![(
                ImageKind::Primary,
                "Show/season-specials-poster.jpg".to_string()
            )]
        );
        assert_eq!(
            found(
                SidecarFormat::Kodi,
                ArtLevel::Season(2026),
                "",
                SHOW,
                &files
            ),
            vec![(
                ImageKind::Backdrop,
                "Show/season2026-fanart.jpg".to_string()
            )]
        );
    }

    #[test]
    fn episode_images_follow_each_format() {
        let stem = "Show - S01E01";
        let files = [
            "Show/Season 01/Show - S01E01-thumb.jpg",
            "Show/Season 01/Show - S01E01.jpg",
            "Show/Season 01/metadata/Show - S01E01.png",
        ];
        assert_eq!(
            found(SidecarFormat::Kodi, ArtLevel::Episode, stem, SHOW, &files),
            vec![(ImageKind::Primary, files[0].to_string())]
        );
        assert_eq!(
            found(SidecarFormat::Plex, ArtLevel::Episode, stem, SHOW, &files),
            vec![(ImageKind::Primary, files[1].to_string())]
        );
        assert_eq!(
            found(
                SidecarFormat::Jellyfin,
                ArtLevel::Episode,
                stem,
                SHOW,
                &files[2..]
            ),
            vec![(ImageKind::Primary, files[2].to_string())]
        );
    }

    #[test]
    fn series_art_and_no_format() {
        let files = ["Show/poster.jpg", "Show/fanart.jpg", "Show/show.jpg"];
        assert_eq!(
            found(
                SidecarFormat::Plex,
                ArtLevel::Series,
                "",
                SHOW,
                &["Show/show.jpg"]
            ),
            vec![(ImageKind::Primary, "Show/show.jpg".to_string())]
        );
        assert_eq!(
            found(SidecarFormat::Kodi, ArtLevel::Series, "", SHOW, &files).len(),
            2
        );
        assert!(
            found(SidecarFormat::None, ArtLevel::Series, "", SHOW, &files).is_empty()
        );
    }
}
