# Remux: local metadata for Local/WebDAV (opendal) addons

Goal: files that have no TMDB/TVDB/IMDb match (recorded sports, home video, events)
still get indexed. They get their metadata and artwork from the media itself
(embedded container tags, embedded cover art) and from adjacent sidecar files. The
user picks the sidecar format: Kodi, Emby/Jellyfin or Plex.

## Status
- **2026-10-24 rev 3.** P1 is implemented and pushed to the fork
  (`northernpowerhouse/remux`, branch `feat/local-metadata`). Format tables are
  sourced (§4.2, §8) and the decisions are made (§7).
- **Work clone:** `projects/dev/remux/remux-local-metadata`, a fresh clone of
  `lostb1t/remux`. This file lives in the branch at `docs/local-metadata-plan.md`;
  drop that commit before opening an upstream PR.
- **Upstream baseline:** `origin/main` @ `e0464330` (2026-10-24). All `file:line`
  references below are against that commit. Copies of the key upstream files and
  the Plex docs are kept outside the repo in
  `projects/dev/remux/local-metadata/` (`*.rs.main`, `refs/`).

### P1 as built
- **Option:** `local_metadata` (Select, `off` | `fallback`, default `off`) on both
  presets. It is ignored for music libraries.
- **Migration** `202610240002_opendal_local_key.sql`: adds `local_key` and
  `match_attempted_at`, plus an index.
- **Shared lookup:** `match_file` replaces the four copies of the id resolution in
  `scan_addon`, and adds the 7-day re-search throttle for local rows.
- **Matched episodes:** `year_season_episode` (`S2026E91`) now beats hunch for every
  episode, matched or not. Hunch read `S2026` as season 202.
- **Catalog, tree, stream and subtitles** select files through
  `OpendalAddon::files_key`, which is the IMDb id or this addon's `local_key`.
- **Stremio:** `opendal::is_local_only` makes Stremio meta, tree, stream and
  subtitle `supports()` skip local-only items.
- **Bug found and fixed:** `scan_addon` marks a file as seen before matching it. A
  skipped file now un-sees itself (`seen_ids.pop()`), so local rows are pruned when
  the option is turned back off.
- **Tests:** 6 new; opendal 58/58; full server suite 1049 passed.

### P2 as built (rev 4, 2026-10-25)
- **Option:** `sidecar_format` (Select, `kodi` | `jellyfin` | `plex` | `none`, default
  `kodi`). It only applies while `local_metadata` is on.
- **Migration** `202610250001_opendal_local_meta.sql`: adds `local_meta` (JSON).
  `local_meta_version` was dropped: the stored ids in `local_meta` are enough to
  tell when the sidecars name a new id.
- **Module `local_meta/`:** `nfo.rs` (Kodi/Jellyfin; BOM, no declaration, several
  roots, HTML entities, trailing or URL-only provider links, unclosed elements) and
  `plexmatch.rs` (`title`/`show`, `year`, `season`, the id keys, `guid`, every
  `ep:` form; `pattern` is deferred). `LocalMeta` is the stored shape.
- **Scan:** sidecars (`*.nfo`, `.plexmatch`) are indexed during the walk, and a pass
  per scan root reads them for the local rows only (`apply_sidecars`):
  - Kodi/Jellyfin: `tvshow.nfo`, `<file>.nfo` (multi-episode joined), `movie.nfo`;
    season names from `<namedseason>`, and for Jellyfin from `season.nfo`.
  - Plex: `.plexmatch` in the series folder (title, ids, `ep:` paths) and in the
    season folder (`season:`, `ep:`).
  - Sidecar numbers beat the file name. Episodes still without numbers are dropped.
  - Movie ids, or series ids from `tvshow.nfo`/`.plexmatch`, that resolve move every
    row with that `local_key` (subtitles too) to the IMDb id. Episode NFO ids are
    never used for the series. Ids are looked up when the TMDB lookup ran this scan
    or the ids changed since the last one.
  - The title column takes the sidecar movie or series title.
- **Read side:** catalog items, seasons (title) and episodes (title, plot, date,
  runtime) carry the metadata. `OpendalAddon` is a `MetaAddon` for its own
  local-only items; `Meta` is in both presets and in `available_info` while
  `local_metadata` is on.
- **Not in P2:** people (dropped by `save_pending_relations` anyway), `<rating>`,
  `lockdata`, a title search with the NFO title (a sports title like "Formula 1"
  would match the wrong show), disc-structure NFOs (`VIDEO_TS.nfo`).
- **Tests:** 12 parser tests, 4 scan tests; opendal 62/62. The full suite passes
  except `create_list_delete_addon_roundtrip`, which needs strem.io (no network in
  the sandbox).

### P3 as built (rev 5, 2026-10-25)
- **Profiles:** `local_meta/artwork.rs` holds the per-format name tables from §4.2
  for movie, series, season and episode art (Primary, Backdrop, Logo, Thumb).
  Extensions: jpg, jpeg, png, webp, tbn. Season names take `NN`, unpadded and
  year forms; `season-specials-*` for season 0; `season-all-*` for Kodi only.
- **NFO links:** `<thumb aspect=…>` (with `type="season" season="N"`) and
  `<fanart><thumb>` (relative to `<fanart url=…>`) remote URLs fill any kind no
  local file covers. Jellyfin's local `<art>` paths are ignored, since the
  name lookup finds those files anyway.
- **Store:** images are indexed during the walk with their listed size and
  mtime, and copied to `{data_dir}/meta/local-art/{sha256}.{ext}` (next to the
  existing `meta/library` uploads, not `metadata/` as first planned). Each
  `LocalImage` keeps its `source` (`path|size|mtime`), so an unchanged file is
  not read again. `local_meta` gains `images` and `season_images`.
- **`sync_from_media`:** also replaces rows whose path is under
  `meta/local-art/` (both separators). `db::LOCAL_ART_DIR` names the folder.
- **GC:** `collect_art_garbage` runs after a scan with `local_metadata` on. It
  keeps copies a `media_images` row or any `local_meta` refers to, and copies
  younger than an hour.
- **Tests:** 4 artwork-table tests, 1 NFO-link test, 1 `sync_from_media` test,
  3 scan tests. Opendal test servers now use a temporary `data_dir`.
- **Pre-existing issue found:** the image endpoint treats a path as local only
  when it starts with `/`, so on Windows uploads and these copies are fetched as
  URLs. Not fixed here.

### Findings that change P2
- **Addon resources.** `available_info` decides an addon's stored resources on
  create and on config change, unless the request names resources. The dashboard's
  Edit dialog always sends its checkboxes. So:
  - P2 adds `Meta` to the preset resources, and to `available_info` when
    `local_metadata` is on.
  - Catalog and tree items also carry the local metadata themselves, so the first
    import is complete even when `Meta` isn't enabled.
  - The `MetaAddon` serves refreshes, including a manual "Refresh metadata".
- **People.** `save_pending_relations` drops people without a TMDB id by design, so
  NFO actors (name-only) are not imported. Genres, studios and tags are.
- **Sidecar reading.** The walk can return a sidecar before or after its video, and
  `tvshow.nfo` after the season folders. So sidecars are collected during the walk
  and applied in a pass after it, for local rows only:
  - NFO ids that resolve turn a local row into an imdb row.
  - NFO season and episode numbers fill rows the filename couldn't number. Rows
    still without numbers are deleted.
- **Storage.** Series- and season-level sidecar data is stored inside each episode
  row's `local_meta` (`series`, `season_title`) instead of a separate
  `opendal_folders` table.
- **Research:** Remux code via DeepWiki (`lostb1t/remux`, index current). Every claim
  the plan depends on was checked against the code, because DeepWiki was vague or
  wrong in places (it could not find `Media::validate`, and it mis-described probe
  storage).
- **No existing upstream issue or PR** covers this (searched: nfo, local metadata,
  no imdb, embedded metadata, fanart, unmatched, sports).

---

## 1. Current behaviour (verified)

### Unmatched files are dropped at scan time
`addons/opendal.rs::scan_addon` (1318) resolves an IMDb id per file in this order:
1. the id already stored for the path (`fetch_existing_imdb`, 2529)
2. Jellyfin-style path ids `[tmdbid-…]`, `[imdbid-…]`, `[tvdbid-…]`
   (`ExternalIds::from_path`, db/media.rs:1126; square brackets only)
3. a TMDB title search on the `hunch`-parsed filename (`resolve_imdb_from_search`)

If none of these produce an id, the file is skipped with `continue`:
- movies: 1883
- episodes: 1833
- subtitles: 1573

### Everything downstream is keyed on `opendal_files.imdb_id`
- `catalog_stream` (≈404) only emits `imdb_id IS NOT NULL` rows (movie/episode).
- `StreamAddon::supports` (742) and `SubtitleAddon::supports` (633) require an imdb id
  (on the series, for episodes).
- `get_streams` looks files up by imdb id.
- `TreeAddon::get_children` (986) builds Season and Episode rows from
  `imdb_id + season/episode`, with ids `season:{imdb}:{s}` and
  `episode:{imdb}:{s}:{e}`.

### Sidecars are never read
Any non-media, non-subtitle extension is skipped silently (≈1608), so `.nfo`, `.jpg`
and `.png` files are ignored. There is a test for this:
`opendal_local_skips_thumbnail_jpg`.

### Embedded metadata is thrown away
- `playback/probe.rs` runs ffprobe with `-show_format -show_streams -show_chapters`.
- `FfprobeFormat` (455) keeps only duration, format_name, bit_rate and size. Format
  tags are not deserialized.
- `FfprobeDisposition.attached_pic` is parsed, but cover art is never extracted.
- `opendal_files.probe_data` stores only a `MediaSourceInfo`.

### `hunch` cannot parse year-numbered seasons
Its patterns are `S(\d{1,3})E…` and `Season\s*(\d{1,2})`. So
`Formula 1 - S2026E91 - …` and the `Season 2026` folder yield no season or episode.
This is Sportarr's (and Plex/Sonarr's) year-season convention.

### Real sample (Sportarr Kodi writer)
In `/store/Sports/Formula 1/Season 2026/`, each episode has its own `<file>.nfo`:
- it starts with a UTF-8 **BOM**
- root element is `<episodedetails>`
- it carries `<season>2026</season>` and `<episode>91</episode>`
- `<aired>`, `<plot>` and `<genre>` are present
- `<uniqueid type="sportarr" default="true">ev-174552</uniqueid>`

There is no `tvshow.nfo` and no artwork yet. Embedded MKV tags hold only `encoder`.

## 2. What already works in our favour (verified)

| Fact | Where | Consequence |
|---|---|---|
| `Media::validate` has no id requirement for Movie/Series (`_ => None`) | db/media.rs:2388 | A row keyed only by `custom_stremio_id` is valid. |
| Local music already indexes id-less items: `custom_stremio_id = "{addon_id}:track:{path}"`, stable UUIDs, with Artist and Album built in `catalog_stream` | opendal.rs ≈459–560 | This is the precedent to copy for movies and series. |
| `refresh_meta` → `resolve_external_ids` resolves only from ids (`/find` via imdb, tvdb or kitsu) and **never title-searches**; `fetch_tmdb_meta` needs `ids.tmdb` | services/resolve.rs:352, addons/tmdb.rs:1219 | TMDB won't attach a wrong match to a locally keyed item. |
| `get_tree` uses the first tree addon that returns non-empty children | addons/mod.rs:2056 | TMDB returns nothing for a local series, so the opendal tree wins. |
| Meta precedence: the first addon in the list wins per field | addons/mod.rs ≈1997 | "Local overrides TMDB" means ordering the local addon above TMDB. No new mechanism needed. |
| The image endpoint serves `/`-prefixed `media_images.path` from disk | api/images.rs:167, `ImageService::serve_local` | Local artwork can be served with no new route. |
| `AddonOptionType::{Select, MultiSelect, Boolean}` exist; the dashboard renders preset options generically | remux-sdks/src/remux/mod.rs:307 | The config needs no dashboard code. |
| `quick-xml 0.37` (with `encoding`) is already a direct dependency of remux-server | crates/remux-server/Cargo.toml:126 | NFO parsing adds no new dependency. |
| `apply_meta` and `locked_fields` already guard Tags, Cast and Genres | addons/mod.rs:565 | User edits stay protected. |

## 3. Gotchas the design must handle

1. **Local image rows are sticky.** `MediaImage::sync_from_media` (db/image.rs:99)
   only replaces a row whose stored path is `LIKE 'http%'`. Absolute paths are
   treated as user uploads and are never replaced. On top of that:
   - the image tag is the row id (`media_image_tag`, api/models.rs:246)
   - the resized-image cache is keyed by row id (`ImageService::process_image`)

   So storing raw sidecar paths would leave stale tags, stale resized copies and
   404 rows. Fix: see §4.5.
2. **WebDAV artwork can't be served by path.** A `/…` path is read from the local
   disk. WebDAV art has to be copied locally.
3. **Orphans.** When a file disappears, its `opendal_files` row is pruned and the
   item is removed from the catalog collection (tasks/catalog_import_shared.rs:494).
   The `media` row itself stays, which is the same as local music today. The live
   check must confirm that orphans don't show up in search.
4. **Stremio addons without `idPrefixes`.** `PickCap` (mod.rs:1276/1321) falls back
   to `supports()`, and `StremioAddon::supports` returns true for any movie or series.
   Such an addon would be sent `opendal:…` ids. Fix: `is_local_only()` makes Stremio
   meta, stream and subtitle `supports()` return false.
5. **Skipped folder names.** `SKIP_DIRS` includes `other` and `specials`, so files
   under such folders vanish. Left alone (Jellyfin-aligned), but documented.
6. **Re-search cost.** An unmatched file already triggers a TMDB title search on every
   scan. Once unmatched files are stored, record `match_attempted_at` and retry
   unmatched rows at most weekly.
7. **Identity follows the path, season and episode.**
   - Renaming or moving a local-only item gives it a new id (watch state is lost),
     the same as local music.
   - Sportarr renumbers episodes when the schedule changes, which also re-keys them.
   - Later improvement, out of scope for v1: key episodes by a non-standard NFO
     `uniqueid` (e.g. `sportarr` `ev-…`) when one is present.
   - If a local item later gains a TMDB match, it moves to the imdb identity and the
     old row becomes an orphan.

## 4. Design

### 4.1 Config (both `opendal-local` and `opendal-webdav` presets)
Strum enums per AGENTS.md. These are new `AddonOption`s next to `media_kind`:

| id | kind | values | default |
|---|---|---|---|
| `local_metadata` | Select | `off` / `fallback` (only files with no TMDB/IMDb match) / `prefer` (also overlay onto matched items) | `off` |
| `sidecar_format` | Select | `kodi` / `jellyfin` (Emby/Jellyfin) / `plex` / `none` | `kodi` |
| `embedded_metadata` | Boolean | read container tags and embedded cover art | `false` |

- **One profile per addon,** because the naming rules conflict:
  - Plex and Jellyfin treat `<file>.jpg` as the episode image or movie poster.
  - Plex's season poster lives in the season folder (`SeasonXX.jpg`), while
    Kodi/Jellyfin's lives in the show folder (`seasonXX-poster.jpg`).
  - Jellyfin's `thumb.jpg` is a landscape Thumb.
- **`off` keeps today's behaviour exactly.**

### 4.2 Format profiles (sources in §8)

#### Text and ids

| | Kodi | Emby/Jellyfin | Plex |
|---|---|---|---|
| Movie | `movie.nfo` (preferred when the folder holds one, unstacked movie), else `<file>.nfo`; disc structures (`VIDEO_TS`, `BDMV`) use the parent folder [K] | `movie.nfo`, `<file>.nfo`, `VIDEO_TS.nfo` [J] | none |
| Series | `tvshow.nfo` (`<tvshow>`) [K] | `tvshow.nfo` [J] | `.plexmatch` in the series folder [P] |
| Season | `<namedseason number="N">` inside `tvshow.nfo` | `season.nfo` in the season folder [J] | `.plexmatch` in the season folder (`season:`) [P] |
| Episode | `<file>.nfo` (`<episodedetails>`) [K] | same; multi-episode = several `<episodedetails>` blocks, titles concatenated, `IndexNumberEnd` = max [J] | `.plexmatch` `ep:` lines [P] |
| IDs | `<uniqueid type=… default=…>`; combination NFO (XML followed by a scraper URL) and URL-only NFO [K] | `uniqueid`, `<imdbid>`, `<tmdbid>`, `<tvdbid>`, `<id>`; URLs after the closing tag parsed for IMDb/TMDB/TVDB ids (`ParseProviderLinks`) [J] | `.plexmatch` `tvdbid`/`tmdbid`/`imdbid` (tt optional)/`guid`; path `{imdb-tt…}`, `{tmdb-N}`, `{tvdb-N}` [P] |

**NFO elements read (Kodi and Jellyfin share one parser)** [J, K, S]:
- **Titles:** `title`, `originaltitle`, `sorttitle`, `showtitle`
- **Text:** `plot` > `outline` > `review`; `tagline`
- **Dates:** `premiered` | `aired` | `releasedate` | `formed`; `year`; `enddate`
- **Classification:** `runtime` (minutes), `mpaa`, `genre`, `studio`, `tag`/`style`
- **People:** `actor` (`name`, `role`, `thumb`, `type`), `director`, `credits`/`writer`
- **Numbering:** `season`, `episode`, `episodenumberend`, `displayseason`,
  `displayepisode`, `namedseason`
- **Status and art:** `status`; `thumb` (with `aspect`) and `fanart/thumb` (remote
  URLs, used only when there is no local image)
- **Locks:** `lockdata` (maps to `is_locked`)

**Plex `.plexmatch` keys** [P]:
- the file is line-based; `#` starts a comment; hint names are case-insensitive
- `title` or `show`, `year`, `season`
- `tvdbid`, `tmdbid`, `imdbid`, `guid` (`source://id`)
- `episode` or `ep` (`S01E01: path`, `SP01: path`, `E02: path`, `02: path`, ranges
  `S01E01-E02`)
- `pattern` or `pt` (globs with `{s}`/`{e}`/`{sp}`/`{see}` tokens): **deferred**
- a series-folder file applies to the folders below it; a season-folder file only
  gives `ep` hints for its own folder

#### Artwork
Accepted extensions:
- Kodi: picture extensions (jpg, png, tbn)
- Jellyfin: png, jpg, jpeg, webp, tbn, gif, svg; we take all except svg and gif
- Plex: jpg, jpeg, png, tbn

| Level / type | Kodi [K, Sn, S] | Emby/Jellyfin [J] | Plex [P] |
|---|---|---|---|
| Movie primary | folder `poster`; `<file>-poster`; legacy `folder.jpg`, `movie.tbn` | folder `poster`/`folder`/`cover`/`default`/`movie`; `<file>-poster` (any primary name as suffix) | `<file>.ext`; folder `cover`/`default`/`folder`/`movie`/`poster`; numbered `-X` |
| Movie backdrop | `fanart`, `<file>-fanart` | `fanart`/`background`/`art`/`backdrop` (+ suffix form, numbered `backdrop-1`/`backdrop2`), `extrafanart/*` | `<file>-fanart.ext`; folder `art`/`backdrop`/`background`/`fanart`; `-X` |
| Movie logo / thumb / banner | `clearlogo` / `landscape` / `banner` (folder or `<file>-…`) | `logo`, `clearlogo` / `landscape`, `thumb` / `banner` | `clearlogo`/`logo` (folder or `<file>-clearlogo`) / – / – |
| Series primary | `poster` | `poster`/`folder`/`cover`/`default`/`show` | `folder`/`poster`/`show` (+ `-X`) |
| Series backdrop | `fanart` | as movie | `art`/`backdrop`/`background`/`fanart` (+ `-X`) |
| Series logo / thumb / banner | `clearlogo` / `landscape` / `banner` | as movie | `clearlogo`/`logo` / – / `banner` |
| Season primary | show folder `seasonNN-poster`, `season-specials-poster`, `season-all-poster` | show folder `seasonNN-poster`, `season-specials-poster` | season folder `SeasonNN.ext` (+ `SeasonNNa`…), specials `season-specials-poster` |
| Season backdrop / banner / thumb | `seasonNN-fanart` / `-banner` / `-landscape` (same `specials`/`all` forms) | `seasonNN-fanart` / `-banner` / `-landscape` | – / season folder `seasonNN-banner` / – |
| Episode image (maps to Primary, as in Jellyfin) | `<file>-thumb` | `<file>-thumb`, `<file>.ext`, `metadata/<file>.ext` | `<file>.ext` |

Name matching is case-insensitive. `NN` is two-digit zero-padded; also accept the
unpadded and 4-digit forms (`season2026-poster`) for year seasons. Theme music
(`theme.mp3`), square art, disc and clear art are not used: Remux has no slot for them.

#### Writers that produce these files (useful for test fixtures) [Sn, S]
- **Sonarr Kodi consumer:**
  - `tvshow.nfo` with `uniqueid` tvdb/imdb/tmdb/tvmaze; can append a TVDB URL after
    the XML
  - episode NFO with `uniqueid` tvdb/sonarr and `fileinfo/streamdetails`
  - `poster.jpg`, `fanart.jpg`, `banner.jpg`, `seasonNN-poster/banner/fanart.jpg`,
    `season-specials-*.jpg`
- **Sonarr Plex consumer:** `.plexmatch` in the series root with `Title`, `Year`,
  `TvdbId`, `ImdbId`, `Episode: S01E01: path`.
- **Sportarr (Kodi only; Plex/Emby/Jellyfin/WDTV not implemented):**
  - `tvshow.nfo` with `title`, `genre`, `uniqueid type="sportarr"` `lg-…`, `plot`,
    `studio`
  - episode NFO with `title`, `showtitle`, `season` (the year), `episode`, `aired`,
    `uniqueid type="sportarr"` `ev-…`, `plot`, `genre`; no `episodeguide`
  - `poster.jpg` and `fanart.jpg` in the league root; `<file>-thumb.jpg`
  - names follow `{Series}/Season {YYYY}/{Series} - S{YYYY}E{N} - [pt{P} - ]{Title} - {Quality}.ext`
  - it also writes a `SPORTARR` MKV global tag

#### Embedded tags
Keys are matched case-insensitively: MKV uses `TITLE`, MP4 uses `title`. In Plex,
the Local Media Assets source reads embedded metadata [P].

| Field | Tags |
|---|---|
| title | `title` |
| overview | `synopsis` > `description` > `summary` > `comment` |
| premiere date | `date_released` > `date` > `date_recorded` (not `creation_time`, which is the mux time) |
| genre | `genre` |
| series | `show` |
| season | `season_number` |
| episode | `episode_sort` |
| cover art | MKV attachment stream with an `image/*` mimetype (prefer `cover*` or `poster*`), or an MP4 stream with `disposition.attached_pic=1` |

### 4.3 Identity for local-only items
This mirrors the music precedent. The `opendal:` prefix lets `is_local_only()` and
the supports() checks stay string checks.

| Kind | Identity |
|---|---|
| Movie | `custom_stremio_id = "opendal:{addon_id}:movie:{path without extension}"` |
| Series | `"opendal:{addon_id}:series:{series dir}"`, where the series dir is the first path component under the scan root (the same fallback `scan_addon` already uses for titles) |
| Season | `season:{series_key}:{s}`, the same shape as the imdb ids the opendal tree already mints |
| Episode | `episode:{series_key}:{s}:{e}` |

### 4.4 Data model (one migration, `2026MMDD0001_opendal_local_metadata.sql`)

**On `opendal_files`:**
- `local_key TEXT`: set for rows with no imdb match while the mode is not `off`.
- `local_meta TEXT`: JSON for `LocalMeta`, holding text fields, dates, lists, people,
  rating, season/episode, ids and image refs.
- `local_meta_version TEXT`: a fingerprint of the sidecar set (name, size and mtime
  of each) plus format and flags.
- `match_attempted_at TEXT`
- index on `(addon_id, media_kind, local_key)`

**New table `opendal_folders`:**
- columns: `addon_id`, `path`, `kind`, `local_meta`, `local_meta_version`,
  `scanned_at`
- primary key `(addon_id, path)`, with ON DELETE CASCADE from `addons`
- holds series and season folder metadata and art once rather than per episode
- pruned with the same seen-set logic as `opendal_files`

### 4.5 Artwork store
Copy every local, WebDAV or embedded image into
`{data_dir}/metadata/local-art/{sha256}.{ext}` (on NVMe, which fits the storage
policy). Store that path in `media_images`.

- A changed poster has a new hash, so a new path, so a fresh row id. That gives a
  new tag and a fresh resized-image cache.
- WebDAV and embedded covers go through the same path.

Extend `sync_from_media`'s guard to
`(path LIKE 'http%' OR path LIKE '{local-art dir}/%')`. Addon-sourced local art is
then replaceable, while user uploads and generated placeholders stay protected.

Garbage collection: after a scan, delete any local-art file that no `media_images`
row references.

### 4.6 Pipeline changes (by file)

1. **`opendal.rs::scan_addon`**
   - **Walk:** record sidecar entries (`.nfo`, `.plexmatch`, images, `metadata/`
     children) in a per-directory map instead of `continue`. Local metadata is
     resolved after the walk, before `prune_stale_paths`.
   - **Id order:**
     1. stored id
     2. path ids: `[…]` plus Plex `{…}`
     3. NFO or `.plexmatch` ids
     4. title search with the sidecar or embedded title and year, if present,
        otherwise the filename
     5. local fallback (`local_key`)
   - **Season/episode order:**
     1. NFO
     2. `.plexmatch` `ep`/`season`
     3. embedded tags
     4. hunch
     5. year-season regexes `S(\d{4})E(\d{1,4})` and `Season (\d{4})`
   - **Re-search throttle:** retry unmatched rows only after 7 days.
2. **`catalog_stream`**: also emit rows that have a `local_key`.
3. **New `MetaAddon` impl on `OpendalAddon`:**
   - add `ResourceType::Meta` to the supported resources; set `meta: Some(addon)`
   - `supports`: local-keyed items (and imdb items in `prefer` mode)
   - `meta_fetch`: builds the patch from `LocalMeta` and `opendal_folders`
   - check that stored addon resources are refreshed on config save (cf. #447)
4. **Tree, Stream and Subtitle impls**: accept `local_key` as well as imdb. Subtitles
   next to unmatched video get the same key.
5. **`playback/probe.rs`**:
   - add format `tags`; surface image attachments and `attached_pic` streams
   - return `EmbeddedMeta`; extract the cover with `-dump_attachment` or
     `-map 0:i -frames:v 1`
   - `embedded_metadata` forces a probe even when `probe_on_scan` is off
6. **`stremio.rs`**: `supports()` returns false for `is_local_only`.
7. **`db/image.rs::sync_from_media`**: the guard change from §4.5.
8. **New module `local_meta/`**: `nfo.rs`, `plexmatch.rs`, `artwork.rs` (profile
   tables) and `embedded.rs`. The NFO parser tolerates:
   - a BOM
   - no XML declaration
   - trailing URL text (ids taken from imdb.com, themoviedb.org and thetvdb.com URLs)
   - URL-only files
   - several root elements
   - HTML entities

### 4.7 Precedence
- **`fallback`:** sidecar > embedded > filename, per field.
- **`prefer`:**
  - order the local addon above TMDB in the addon list (first wins per field)
  - ids are never overridden
  - `locked_fields` are respected
  - an NFO title also beats a per-user translation (fork PR #5); revisit when P6 lands

## 5. Phases

| Phase | Scope | Unblocks |
|---|---|---|
| P1 (done) | `local_metadata` option; index unmatched files under `local_key`; catalog, tree, stream and subtitle by key; year-season regex; Stremio skip; re-search throttle. Titles come from filename and folder. | Sports show up at all |
| P2 (done) | NFO parser (Kodi/Jellyfin) and `.plexmatch`; `opendal_folders`; sidecar ids feed matching; opendal `MetaAddon` | Titles, plots, dates, genres |
| P3 (done) | Artwork profiles for all three formats; local-art store; `sync_from_media` guard; GC | Posters, fanart, thumbs |
| P4 | Plex `{…}` path ids (if not folded into P1) | Plex-organised libraries |
| P5 | `embedded_metadata`: format tags and cover art via the probe | DVR recordings, tagged MP4s |
| P6 | `prefer` mode | Curated NFOs beat TMDB |

## 6. Tests
- **NFO fixtures:**
  - Kodi `movie.nfo`, `tvshow.nfo` (with `namedseason`), multi-episode
    `episodedetails`
  - Jellyfin NFO with `lockdata` and `<imdbid>`
  - BOM with a trailing TVDB URL (the Sonarr style); URL-only NFO
  - the Sportarr sample verbatim
- **`.plexmatch` fixtures:** comments, mixed case, `ep:` forms, the Sonarr output
  format.
- **Artwork:** table-driven resolution per profile and level.
- **opendal tests:** extend the existing style per mode (`off` still skips; `fallback`
  indexes). Add a tree, stream and subtitle round-trip for a local-keyed series.
- **ffmpeg-gated tests** for embedded tags and cover extraction.
- **`sync_from_media`:** a local-art row is replaceable; an uploaded `/…` row is not.
- **Live check** (see the `remux_livecheck_recipe` memory): layered image on a free
  port, with `/store/Sports` mounted `:ro`.

## 7. Decisions (2026-10-24, user accepted the recommendations)
- **Q1:** `local_metadata` defaults to `off`.
- **Q2:** one `sidecar_format` per addon (a single Select).
- **Q3:** date-based episodes are deferred. Plex documents `ShowName – 2011-11-15 –
  Info.ext` (YYYY-MM-DD or DD-MM-YYYY) [P]; that's the target if it's added later.
- **Q4:** a content-addressed copy store for artwork.
- **Q5:** build on a fresh upstream clone first. Draft the upstream issue text locally
  (`ISSUE.md`); nothing is posted until the user says so.

## 8. Sources
- **[K] Kodi** (DeepWiki `xbmc/xbmc`):
  - `CVideoTagLoaderNFO::FindNFO` and `CNfoFile` (`COMBINED`/`URL` parse results)
  - `CVideoThumbLoader::GetLocalArt`/`GetArtTypes` (`artTypeDefaults`: movie
    poster+fanart; tvshow and season poster+fanart+banner; episode thumb)
  - season `seasonNN-*`, `season-specials-*`, `season-all-*`; episode `<file>-thumb`
- **[J] Jellyfin** (DeepWiki `jellyfin/jellyfin`, plus jellyfin.org/docs
  `metadata/nfo` and `media/shows`):
  - `LocalImageProvider` name lists and `PopulateSeasonImagesFromSeriesFolder`
  - `EpisodeLocalImageProvider`
  - `BaseItem.SupportedImageExtensions`
  - the `BaseNfoParser` element map, `SupportsUrlAfterClosingXmlTag` +
    `ParseProviderLinks`, `EpisodeNfoParser` multi-episode
- **[P] Plex** (support.plex.tv, saved outside the repo in
  `projects/dev/remux/local-metadata/refs/plex-*.txt`):
  - "Local Media Assets – Movies" and "– TV Shows"
  - "Plexmatch"
  - "Naming and organizing your Movie / TV Show files" (`{tmdb-…}` ids, date-based
    episodes)
  - DeepWiki has no index of Plex's agents. The GitHub mirror
    `HooliganHarls/LocalMedia.bundle` (`refs/plex-localmedia/__init__.py`) is an old
    movie-only fork that agrees with the movie rules.
- **[Sn] Sonarr** (DeepWiki `Sonarr/Sonarr`): `XbmcMetadata` and `PlexMetadata`
  consumers.
- **[S] Sportarr** (DeepWiki `Sportarr/Sportarr`): `MetadataWriterService`, the naming
  template, and the Plex legacy bundle and Emby plugin id regex
  `(^|[^a-z0-9])(sportarr[-._ ]+)?(ev|lg)[-._ ]*\d{4,10}(?![0-9])`.
