//! Jellyfin DVR (Timers / SeriesTimers / Recordings) backed by Dispatcharr.
//!
//! Timers and series rules are forwarded live and never persisted; recordings
//! are synced into `media` as `MediaKind::Recording` rows so playback can use
//! the generic item pipeline.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    AppContext,
    addons::{dispatcharr, dispatcharr_dvr},
    api, db,
};

#[derive(Debug, Clone)]
pub struct DvrConfig {
    pub addon_id: Uuid,
    pub base_url: String,
    pub api_key: String,
}

/// A timer as a client addresses it. Dispatcharr numbers recordings per
/// instance, so the id has to name the instance too or a cancel lands on
/// whichever recording another instance happens to number the same.
/// Serialised as `{addon}:{recording}` — Jellyfin timer ids are opaque
/// strings, so the shape is ours to choose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerId {
    pub addon_id: Uuid,
    pub recording_id: i64,
}

impl std::fmt::Display for TimerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}",
            self.addon_id
                .simple(),
            self.recording_id
        )
    }
}

impl std::str::FromStr for TimerId {
    type Err = ();

    fn from_str(raw: &str) -> std::result::Result<Self, Self::Err> {
        let (addon, recording) = raw
            .split_once(':')
            .ok_or(())?;
        Ok(Self {
            addon_id: Uuid::parse_str(addon).map_err(|_| ())?,
            recording_id: recording
                .parse()
                .map_err(|_| ())?,
        })
    }
}

pub struct DvrService;

impl DvrService {
    /// Every enabled `dispatcharr`-kind addon instance that is configured
    /// well enough to reach, in addon order.
    pub fn configs(ctx: &AppContext) -> Vec<DvrConfig> {
        ctx.addons
            .list()
            .iter()
            .filter(|r| {
                r.row
                    .enabled
                    && r.row
                        .preset
                        .kind
                        == "dispatcharr"
            })
            .filter_map(|runtime| {
                let config = runtime
                    .row
                    .preset
                    .config
                    .expose();
                Some(DvrConfig {
                    addon_id: runtime
                        .row
                        .id,
                    base_url: config["base_url"]
                        .as_str()
                        .filter(|s| !s.is_empty())?
                        .trim_end_matches('/')
                        .to_string(),
                    api_key: config["api_key"]
                        .as_str()
                        .filter(|s| !s.is_empty())?
                        .to_string(),
                })
            })
            .collect()
    }

    /// First enabled `dispatcharr`-kind addon instance. Only for operations
    /// with nothing to route by — creating a timer, where Jellyfin's DVR API
    /// carries no provider. Anything acting on an existing row must resolve
    /// that row's own instance instead.
    pub async fn active_config(ctx: &AppContext) -> Option<DvrConfig> {
        Self::configs(ctx)
            .into_iter()
            .next()
    }

    pub fn config_for_addon(ctx: &AppContext, addon_id: Uuid) -> Option<DvrConfig> {
        Self::configs(ctx)
            .into_iter()
            .find(|cfg| cfg.addon_id == addon_id)
    }

    /// The instance a synced row came from, through the `iptv_source_id` its
    /// sync stamped on it. Deliberately no fallback when that instance is
    /// gone: sending one instance's numeric recording id to another deletes
    /// or plays an unrelated recording that happens to share it.
    pub fn config_for_media(ctx: &AppContext, media: &db::Media) -> Option<DvrConfig> {
        let addon_id = media
            .external_ids
            .iptv_source_id
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok())?;
        Self::config_for_addon(ctx, addon_id)
    }

    async fn resolve_channel_int(
        cfg: &DvrConfig,
        channel_uuid: Uuid,
    ) -> Result<Option<i64>> {
        let client = dispatcharr::CLIENT.clone();
        let channels =
            dispatcharr::fetch_channels(&client, &cfg.base_url, &cfg.api_key).await?;
        Ok(channels
            .into_iter()
            .find(|ch| channel_uuid_of(cfg.addon_id, ch.id) == channel_uuid)
            .map(|ch| ch.id))
    }

    /// The channel's *authoritative* `tvg_id`, resolved live via
    /// `epg_data_id -> EPGData.tvg_id` — not a cached copy, which
    /// `RefreshDispatcharrLiveTvTask` already documents can go stale.
    async fn resolve_channel_tvg_id(
        cfg: &DvrConfig,
        channel_id: i64,
    ) -> Result<Option<String>> {
        let client = dispatcharr::CLIENT.clone();
        let channels =
            dispatcharr::fetch_channels(&client, &cfg.base_url, &cfg.api_key).await?;
        let Some(epg_data_id) = channels
            .into_iter()
            .find(|ch| ch.id == channel_id)
            .and_then(|ch| ch.epg_data_id)
        else {
            return Ok(None);
        };
        let epg_data =
            dispatcharr::fetch_epg_data(&client, &cfg.base_url, &cfg.api_key).await?;
        Ok(epg_data
            .into_iter()
            .find(|d| d.id == epg_data_id)
            .and_then(|d| d.tvg_id))
    }

    // -- Timers --------------------------------------------------------

    pub async fn list_timers(ctx: &AppContext) -> Result<Vec<TimerInfoDto>> {
        let client = dispatcharr::CLIENT.clone();
        let mut timers: Vec<TimerInfoDto> = Vec::new();
        for cfg in Self::configs(ctx) {
            // One unreachable instance must not blank the others' timers.
            let recordings = match dispatcharr_dvr::list_recordings(
                &client,
                &cfg.base_url,
                &cfg.api_key,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(addon = %cfg.addon_id, error = %e, "failed to list Dispatcharr timers");
                    continue;
                }
            };
            timers.extend(
                recordings
                    .iter()
                    .filter(|r| {
                        matches!(
                            r.status(),
                            dispatcharr_dvr::DispatcharrRecordingStatus::Scheduled
                                | dispatcharr_dvr::DispatcharrRecordingStatus::Recording
                        )
                    })
                    .map(|r| timer_from_recording(cfg.addon_id, r)),
            );
        }
        timers.sort_by_key(|t| t.start_date);
        Ok(timers)
    }

    pub async fn get_timer(
        ctx: &AppContext,
        id: TimerId,
    ) -> Result<Option<TimerInfoDto>> {
        let Some(cfg) = Self::config_for_addon(ctx, id.addon_id) else {
            return Ok(None);
        };
        let client = dispatcharr::CLIENT.clone();
        match dispatcharr_dvr::get_recording(
            &client,
            &cfg.base_url,
            &cfg.api_key,
            id.recording_id,
        )
        .await
        {
            Ok(rec) => Ok(Some(timer_from_recording(cfg.addon_id, &rec))),
            Err(_) => Ok(None),
        }
    }

    pub async fn create_timer(
        ctx: &AppContext,
        req: CreateTimerRequest,
    ) -> Result<TimerInfoDto> {
        let (channel_uuid, mut start, mut end) = if let Some(pid) = req.program_id {
            let program = db::Media::get_by_id(&ctx.db, &pid)
                .await?
                .context("program not found")?;
            let channel = program
                .parent_id
                .context("program has no parent channel")?;
            let start = req
                .start_date
                .or_else(|| {
                    program
                        .live_start
                        .map(|d| d.and_utc())
                })
                .context("program has no start time")?;
            let end = req
                .end_date
                .or_else(|| {
                    program
                        .live_end
                        .map(|d| d.and_utc())
                })
                .unwrap_or(start + Duration::hours(1));
            (channel, start, end)
        } else {
            (
                req.channel_id
                    .context("ChannelId or ProgramId is required")?,
                req.start_date
                    .context("StartDate is required")?,
                req.end_date
                    .context("EndDate is required")?,
            )
        };

        start -= Duration::seconds(req.pre_padding_seconds as i64);
        end += Duration::seconds(req.post_padding_seconds as i64);

        let channel = db::Media::get_by_id(&ctx.db, &channel_uuid)
            .await?
            .context("channel not found")?;
        let cfg = Self::config_for_media(ctx, &channel)
            .context("channel's Dispatcharr instance not configured")?;

        let channel_id = Self::resolve_channel_int(&cfg, channel_uuid)
            .await?
            .context("channel not found on Dispatcharr")?;
        let rec = dispatcharr_dvr::schedule_recording(
            &dispatcharr::CLIENT,
            &cfg.base_url,
            &cfg.api_key,
            channel_id,
            start,
            end,
        )
        .await?;
        Ok(timer_from_recording(cfg.addon_id, &rec))
    }

    /// The instance and recording a timer id as a client sends it refers to:
    /// our own `{addon}:{recording}` form, a bare number from a build that
    /// predates it, or the UUID of the in-progress `Recording` item a timer
    /// points at through `ProgramInfo` (some clients cancel by
    /// `ProgramInfo.Id`).
    pub async fn resolve_timer_id(ctx: &AppContext, raw: &str) -> Option<TimerId> {
        if let Ok(id) = raw.parse::<TimerId>() {
            return Some(id);
        }
        if let Ok(recording_id) = raw.parse::<i64>() {
            // Only an id a client cached before timers carried their
            // instance, so the single-instance reading is the right one.
            return Self::active_config(ctx)
                .await
                .map(|cfg| TimerId {
                    addon_id: cfg.addon_id,
                    recording_id,
                });
        }
        let uuid = raw
            .parse::<Uuid>()
            .ok()?;
        if let Some(media) = db::Media::get_by_id(&ctx.db, &uuid)
            .await
            .ok()
            .flatten()
        {
            if media.kind != db::MediaKind::Recording {
                return None;
            }
            let cfg = Self::config_for_media(ctx, &media)?;
            return media
                .external_ids
                .dispatcharr_recording_id
                .map(|recording_id| TimerId {
                    addon_id: cfg.addon_id,
                    recording_id,
                });
        }
        // Not synced yet (a client can cancel from the schedule before it ever
        // lists recordings): the id is derived from Dispatcharr's own, so match
        // it against each instance's live list.
        for cfg in Self::configs(ctx) {
            let Ok(recordings) = dispatcharr_dvr::list_recordings(
                &dispatcharr::CLIENT,
                &cfg.base_url,
                &cfg.api_key,
            )
            .await
            else {
                continue;
            };
            if let Some(recording_id) =
                recording_id_for_uuid(&recordings, cfg.addon_id, uuid)
            {
                return Some(TimerId {
                    addon_id: cfg.addon_id,
                    recording_id,
                });
            }
        }
        None
    }

    /// Cancels the timer: stops it if it's currently recording (keeping the
    /// partial file), otherwise deletes the not-yet-started scheduled entry.
    pub async fn delete_timer(ctx: &AppContext, id: TimerId) -> Result<bool> {
        let Some(cfg) = Self::config_for_addon(ctx, id.addon_id) else {
            return Ok(false);
        };
        let client = dispatcharr::CLIENT.clone();
        let rec_id = id.recording_id;
        let Ok(rec) = dispatcharr_dvr::get_recording(
            &client,
            &cfg.base_url,
            &cfg.api_key,
            rec_id,
        )
        .await
        else {
            return Ok(false);
        };
        if rec.status() == dispatcharr_dvr::DispatcharrRecordingStatus::Recording {
            dispatcharr_dvr::stop_recording(
                &client,
                &cfg.base_url,
                &cfg.api_key,
                rec_id,
            )
            .await?;
        } else {
            dispatcharr_dvr::delete_recording(
                &client,
                &cfg.base_url,
                &cfg.api_key,
                rec_id,
            )
            .await?;
        }
        Ok(true)
    }

    // -- SeriesTimers ----------------------------------------------------

    pub async fn list_series_timers(
        ctx: &AppContext,
    ) -> Result<Vec<SeriesTimerInfoDto>> {
        let client = dispatcharr::CLIENT.clone();
        let mut timers = Vec::new();
        for cfg in Self::configs(ctx) {
            // One unreachable instance must not blank the others' rules.
            match dispatcharr_dvr::list_series_rules(
                &client,
                &cfg.base_url,
                &cfg.api_key,
            )
            .await
            {
                Ok(rules) => timers.extend(
                    rules
                        .iter()
                        .map(|r| series_timer_from_rule(cfg.addon_id, r)),
                ),
                Err(e) => {
                    tracing::warn!(addon = %cfg.addon_id, error = %e, "failed to list Dispatcharr series rules");
                }
            }
        }
        Ok(timers)
    }

    /// `SeriesTimerInfoDto::id` is percent-encoded, but `id` here comes from
    /// a URL path segment that axum has already decoded, so the two can't be
    /// compared as raw strings — `parse_series_timer_id` normalizes both
    /// sides before comparing.
    pub async fn get_series_timer(
        ctx: &AppContext,
        id: &str,
    ) -> Result<Option<SeriesTimerInfoDto>> {
        let Some(target) = parse_series_timer_id(id) else {
            return Ok(None);
        };
        let items = Self::list_series_timers(ctx).await?;
        Ok(items
            .into_iter()
            .find(|t| parse_series_timer_id(&t.id) == Some(target.clone())))
    }

    pub async fn create_series_timer(
        ctx: &AppContext,
        req: CreateSeriesTimerRequest,
    ) -> Result<SeriesTimerInfoDto> {
        let (channel_uuid, name) = if let Some(pid) = req.program_id {
            let program = db::Media::get_by_id(&ctx.db, &pid)
                .await?
                .context("program not found")?;
            let channel = program
                .parent_id
                .context("program has no parent channel")?;
            let name = req
                .name
                .unwrap_or(program.title);
            (channel, name)
        } else {
            (
                req.channel_id
                    .context("ChannelId or ProgramId is required")?,
                req.name
                    .context(
                        "Name is required when ChannelId is given without ProgramId",
                    )?,
            )
        };

        let channel = db::Media::get_by_id(&ctx.db, &channel_uuid)
            .await?
            .context("channel not found")?;
        let cfg = Self::config_for_media(ctx, &channel)
            .context("channel's Dispatcharr instance not configured")?;

        let channel_id = Self::resolve_channel_int(&cfg, channel_uuid)
            .await?
            .context("channel not found on Dispatcharr")?;
        let tvg_id = Self::resolve_channel_tvg_id(&cfg, channel_id)
            .await?
            .context("channel has no EPG mapping on Dispatcharr")?;

        let request = dispatcharr_dvr::SeriesRuleRequest {
            tvg_id: Some(tvg_id),
            mode: if req.record_new_only { "new" } else { "all" }.to_string(),
            title: Some(name),
            epg_source_id: None,
        };
        let rules = dispatcharr_dvr::create_series_rule(
            &dispatcharr::CLIENT,
            &cfg.base_url,
            &cfg.api_key,
            &request,
        )
        .await?;
        let created = rules
            .iter()
            .find(|r| r.tvg_id == request.tvg_id && r.title == request.title)
            .or_else(|| rules.last())
            .context("Dispatcharr returned no rules after create")?;
        Ok(series_timer_from_rule(cfg.addon_id, created))
    }

    pub async fn delete_series_timer(ctx: &AppContext, id: &str) -> Result<bool> {
        let Some((addon_id, tvg_id, title, epg_source_id)) = parse_series_timer_id(id)
        else {
            return Ok(false);
        };
        let cfg = match addon_id {
            Some(addon_id) => Self::config_for_addon(ctx, addon_id),
            // An id cached before series timers carried their instance.
            None => Self::active_config(ctx).await,
        };
        let Some(cfg) = cfg else {
            return Ok(false);
        };
        dispatcharr_dvr::delete_series_rule(
            &dispatcharr::CLIENT,
            &cfg.base_url,
            &cfg.api_key,
            tvg_id.as_deref(),
            title.as_deref(),
            epg_source_id,
        )
        .await?;
        Ok(true)
    }

    // -- Recordings ------------------------------------------------------
    //
    // `RefreshDispatcharrLiveTvTask` runs every few hours by default, so
    // every read here re-syncs live first via `sync_recordings` — a
    // Dispatcharr `list_recordings()` call is cheap for a homelab-sized DVR.

    /// Makes sure every channel row the given recordings hang off exists,
    /// importing the ones that are missing. Returns the channel ids that are
    /// actually present afterwards — a recording on a channel Dispatcharr no
    /// longer lists has no row to hang off and cannot be synced.
    async fn ensure_channel_parents(
        ctx: &AppContext,
        cfg: &DvrConfig,
        recordings: &[dispatcharr_dvr::DispatcharrRecording],
    ) -> Result<std::collections::HashSet<Uuid>> {
        let wanted: std::collections::HashSet<Uuid> = recordings
            .iter()
            .map(|r| channel_uuid_of(cfg.addon_id, r.channel))
            .collect();
        if wanted.is_empty() {
            return Ok(wanted);
        }

        let wanted_ids: Vec<Uuid> = wanted
            .iter()
            .copied()
            .collect();
        let mut present: std::collections::HashSet<Uuid> =
            std::collections::HashSet::new();
        for chunk in wanted_ids.chunks(500) {
            let mut qb = sqlx::QueryBuilder::new("SELECT id FROM media WHERE id IN (");
            let mut sep = qb.separated(", ");
            for id in chunk {
                sep.push_bind(id);
            }
            qb.push(")");
            let found: Vec<Uuid> = qb
                .build_query_scalar()
                .fetch_all(&ctx.db)
                .await?;
            present.extend(found);
        }
        if present.len() == wanted.len() {
            return Ok(present);
        }

        let source_id = cfg
            .addon_id
            .simple()
            .to_string();
        let channels = dispatcharr::fetch_channels(
            &dispatcharr::CLIENT,
            &cfg.base_url,
            &cfg.api_key,
        )
        .await?;
        let missing: Vec<db::Media> = channels
            .iter()
            .map(|ch| dispatcharr::channel_to_media(ch, cfg.addon_id, &source_id))
            .filter(|m| wanted.contains(&m.id) && !present.contains(&m.id))
            .collect();
        present.extend(
            missing
                .iter()
                .map(|m| m.id),
        );
        db::Media::upsert(&ctx.db, &missing).await?;
        Ok(present)
    }

    /// Fetches current recordings from Dispatcharr and upserts/prunes the
    /// synced `Recording` rows (+ their `Stream` children) for one addon.
    /// Shared by the periodic task and every on-access read below — the
    /// task calls this once per configured Dispatcharr addon; on-access
    /// reads call it once for `active_config()`'s single addon.
    pub async fn sync_recordings(ctx: &AppContext, cfg: &DvrConfig) -> Result<usize> {
        let client = dispatcharr::CLIENT.clone();
        let source_id = cfg
            .addon_id
            .simple()
            .to_string();

        let mut recordings =
            dispatcharr_dvr::list_recordings(&client, &cfg.base_url, &cfg.api_key)
                .await?;
        // Not-yet-started recordings have no file on Dispatcharr's side yet,
        // so syncing them as a playable `Recording` row would make them
        // appear watchable when they aren't — they already surface
        // correctly as Timers.
        recordings.retain(|r| {
            !matches!(
                r.status(),
                dispatcharr_dvr::DispatcharrRecordingStatus::Scheduled
            )
        });

        // Each row is parented to its channel row by FK, and nothing orders
        // this after a channel import: a freshly configured addon read
        // on demand has no channels yet, and a recording can outlive the
        // channel it was made on. A missing parent fails the whole upsert at
        // commit, taking every other recording with it.
        let parents = Self::ensure_channel_parents(ctx, cfg, &recordings).await?;
        let before = recordings.len();
        recordings
            .retain(|r| parents.contains(&channel_uuid_of(cfg.addon_id, r.channel)));
        if recordings.len() < before {
            tracing::warn!(
                addon = %cfg.addon_id,
                dropped = before - recordings.len(),
                "skipping Dispatcharr recordings whose channel no longer exists"
            );
        }

        let recording_rows: Vec<db::Media> = recordings
            .iter()
            .map(|rec| dispatcharr::recording_to_media(rec, cfg.addon_id, &source_id))
            .collect();
        db::Media::upsert(&ctx.db, &recording_rows).await?;

        // Parent rows must land first (FK on `parent_id`).
        let now = chrono::Utc::now().naive_utc();
        let stream_rows: Vec<db::Media> = recordings
            .iter()
            .zip(recording_rows.iter())
            .map(|(rec, media)| {
                dispatcharr::recording_stream_to_media(
                    rec,
                    media.id,
                    &dispatcharr::recording_stream_url(
                        ctx.config
                            .port,
                        media.id,
                    ),
                    now,
                )
            })
            .collect();
        db::Media::upsert(&ctx.db, &stream_rows).await?;

        let keep_ids: Vec<Uuid> = recording_rows
            .iter()
            .map(|m| m.id)
            .collect();
        let mut qb = sqlx::QueryBuilder::new(
            "DELETE FROM media WHERE kind = 'recording' AND json_extract(external_ids, '$.iptv_source_id') = ",
        );
        qb.push_bind(&source_id);
        if !keep_ids.is_empty() {
            qb.push(" AND id NOT IN (");
            let mut sep = qb.separated(", ");
            for id in &keep_ids {
                sep.push_bind(id);
            }
            qb.push(")");
        }
        qb.build()
            .execute(&ctx.db)
            .await?;

        Ok(recordings.len())
    }

    pub async fn list_recordings(ctx: &AppContext) -> Result<Vec<api::BaseItemDto>> {
        for cfg in Self::configs(ctx) {
            if let Err(e) = Self::sync_recordings(ctx, &cfg).await {
                tracing::warn!(addon = %cfg.addon_id, error = %e, "failed to refresh recordings from Dispatcharr, serving last-synced state");
            }
        }
        let mut result = db::Media::get_by_filter(
            &ctx.db,
            &db::MediaFilter {
                kind: Some(vec![db::MediaKind::Recording]),
                ..Default::default()
            },
        )
        .await?;
        db::Media::attach_streams(&ctx.db, &mut result.records).await?;
        let mut items: Vec<api::BaseItemDto> = result
            .records
            .into_iter()
            .map(|m| api::db_media_to_item(m, false))
            .collect();
        newest_first(&mut items);
        Ok(items)
    }

    pub async fn get_recording(
        ctx: &AppContext,
        id: Uuid,
    ) -> Result<Option<api::BaseItemDto>> {
        if db::Media::get_by_id(&ctx.db, &id)
            .await?
            .is_none()
        {
            // Not synced yet (e.g. it just finished) — try once before
            // giving up, rather than always paying the round trip. The id
            // says nothing about which instance owns it, so try each.
            for cfg in Self::configs(ctx) {
                let _ = Self::sync_recordings(ctx, &cfg).await;
            }
        }
        let Some(mut media) = db::Media::get_by_id(&ctx.db, &id).await? else {
            return Ok(None);
        };
        if media.kind != db::MediaKind::Recording {
            return Ok(None);
        }
        media.sources = Some(
            media
                .streams(&ctx.db)
                .await?,
        );
        Ok(Some(api::db_media_to_item(media, false)))
    }

    /// If the recording's own instance is no longer configured, only the
    /// local row is removed — the id is only meaningful on that instance, so
    /// there is no other instance it is safe to send a delete to. The
    /// upstream recording can reappear on the next sync if that instance is
    /// reconfigured.
    pub async fn delete_recording(ctx: &AppContext, id: Uuid) -> Result<bool> {
        let Some(media) = db::Media::get_by_id(&ctx.db, &id).await? else {
            return Ok(false);
        };
        if media.kind != db::MediaKind::Recording {
            return Ok(false);
        }
        if let (Some(dispatcharr_id), Some(cfg)) = (
            media
                .external_ids
                .dispatcharr_recording_id,
            Self::config_for_media(ctx, &media),
        ) {
            dispatcharr_dvr::delete_recording(
                &dispatcharr::CLIENT,
                &cfg.base_url,
                &cfg.api_key,
                dispatcharr_id,
            )
            .await?;
        }
        db::Media::delete(&ctx.db, &id).await?;
        Ok(true)
    }

    /// Resolves a synced recording to what the stream proxy needs: the
    /// Dispatcharr config to reach it with, plus its *current*
    /// `DispatcharrRecording` (fetched live — `file_url`/status change as a
    /// recording moves from in-progress to finished, so the DB row, synced
    /// only periodically, can't be trusted for this).
    pub async fn recording_playback_target(
        ctx: &AppContext,
        id: Uuid,
    ) -> Result<Option<(DvrConfig, dispatcharr_dvr::DispatcharrRecording)>> {
        let Some(media) = db::Media::get_by_id(&ctx.db, &id).await? else {
            return Ok(None);
        };
        let Some(dispatcharr_id) = media
            .external_ids
            .dispatcharr_recording_id
        else {
            return Ok(None);
        };
        let Some(cfg) = Self::config_for_media(ctx, &media) else {
            return Ok(None);
        };
        let rec = dispatcharr_dvr::get_recording(
            &dispatcharr::CLIENT,
            &cfg.base_url,
            &cfg.api_key,
            dispatcharr_id,
        )
        .await?;
        Ok(Some((cfg, rec)))
    }
}

/// Newest recording first, the order clients expect for "recent recordings"
/// (Moonfin shows the list as given). Done here because the DB layer has no
/// sort key for a recording's start: `ItemSortBy::StartDate` is not handled
/// there, so asking it to sort was silently a no-op. Items without a
/// parseable start go last.
fn newest_first(items: &mut [api::BaseItemDto]) {
    let start = |i: &api::BaseItemDto| {
        i.start_date
            .as_deref()
            .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
    };
    items.sort_by(|a, b| start(b).cmp(&start(a)));
}

/// Dispatcharr's id of the recording whose synced item id is `uuid`.
fn recording_id_for_uuid(
    recordings: &[dispatcharr_dvr::DispatcharrRecording],
    addon_id: Uuid,
    uuid: Uuid,
) -> Option<i64> {
    recordings
        .iter()
        .find(|r| dispatcharr::recording_media_id(addon_id, r.id) == uuid)
        .map(|r| r.id)
}

fn channel_uuid_of(addon_id: Uuid, channel_id: i64) -> Uuid {
    Uuid::new_v5(&addon_id, format!("channel:{channel_id}").as_bytes())
}

fn timer_from_recording(
    addon_id: Uuid,
    rec: &dispatcharr_dvr::DispatcharrRecording,
) -> TimerInfoDto {
    TimerInfoDto {
        id: TimerId {
            addon_id,
            recording_id: rec.id,
        }
        .to_string(),
        type_: "Timer".to_string(),
        server_id: crate::common::server_id(),
        channel_id: Some(channel_uuid_of(addon_id, rec.channel)),
        program_id: None,
        name: rec
            .program_title()
            .unwrap_or("Recording")
            .to_string(),
        overview: rec
            .program_description()
            .map(str::to_owned),
        start_date: rec.start_time,
        end_date: rec.end_time,
        service_name: "dispatcharr".to_string(),
        priority: 0,
        pre_padding_seconds: 0,
        post_padding_seconds: 0,
        is_pre_padding_required: false,
        is_post_padding_required: false,
        status: RecordingStatus::from(rec.status()),
        series_timer_id: None,
        program_info: (rec.status()
            == dispatcharr_dvr::DispatcharrRecordingStatus::Recording)
            .then(|| {
                api::db_media_to_item(
                    dispatcharr::recording_to_media(
                        rec,
                        addon_id,
                        &addon_id
                            .simple()
                            .to_string(),
                    ),
                    false,
                )
            }),
    }
}

fn series_timer_id(
    addon_id: Uuid,
    rule: &dispatcharr_dvr::DispatcharrSeriesRule,
) -> String {
    let raw = format!(
        "{}|{}|{}|{}",
        addon_id.simple(),
        rule.tvg_id
            .as_deref()
            .unwrap_or(""),
        rule.title
            .as_deref()
            .unwrap_or(""),
        rule.epg_source_id
            .map(|v| v.to_string())
            .unwrap_or_default(),
    );
    urlencoding::encode(&raw).into_owned()
}

/// `(addon, tvg_id, title, epg_source_id)`. The addon is absent for an id a
/// client cached before series timers carried the instance they live on.
fn parse_series_timer_id(
    id: &str,
) -> Option<(Option<Uuid>, Option<String>, Option<String>, Option<i64>)> {
    let decoded = urlencoding::decode(id).ok()?;
    let (addon_id, decoded) = match decoded
        .split_once('|')
        .and_then(|(head, rest)| {
            Uuid::parse_str(head)
                .ok()
                .map(|addon| (addon, rest))
        }) {
        Some((addon, rest)) => (Some(addon), std::borrow::Cow::Owned(rest.to_owned())),
        None => (None, decoded),
    };
    // The title is free text and may itself contain `|`, so it is whatever sits
    // between the first separator (after `tvg_id`) and the last (before the
    // numeric `epg_source_id`).
    let (tvg_id, rest) = decoded
        .split_once('|')
        .unwrap_or((&decoded, ""));
    let (title, epg_source_id) = rest
        .rsplit_once('|')
        .unwrap_or((rest, ""));
    let tvg_id = Some(tvg_id)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let title = Some(title)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let epg_source_id = Some(epg_source_id)
        .filter(|s| !s.is_empty())
        .and_then(|s| {
            s.parse()
                .ok()
        });
    Some((addon_id, tvg_id, title, epg_source_id))
}

fn series_timer_from_rule(
    addon_id: Uuid,
    rule: &dispatcharr_dvr::DispatcharrSeriesRule,
) -> SeriesTimerInfoDto {
    SeriesTimerInfoDto {
        id: series_timer_id(addon_id, rule),
        type_: "SeriesTimer".to_string(),
        server_id: crate::common::server_id(),
        name: rule
            .title
            .clone()
            .unwrap_or_else(|| "All programs".to_string()),
        overview: rule
            .description
            .clone(),
        service_name: "dispatcharr".to_string(),
        priority: 0,
        record_any_time: true,
        record_any_channel: rule
            .tvg_id
            .is_none(),
        record_new_only: rule.mode == "new",
        skip_episodes_in_library: false,
    }
}

// ---------------------------------------------------------------------------
// Wire DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum RecordingStatus {
    New,
    InProgress,
    Completed,
    Cancelled,
    Error,
}

impl From<dispatcharr_dvr::DispatcharrRecordingStatus> for RecordingStatus {
    fn from(s: dispatcharr_dvr::DispatcharrRecordingStatus) -> Self {
        use dispatcharr_dvr::DispatcharrRecordingStatus as S;
        match s {
            S::Scheduled => RecordingStatus::New,
            S::Recording => RecordingStatus::InProgress,
            S::Completed => RecordingStatus::Completed,
            S::Stopped | S::Interrupted => RecordingStatus::Cancelled,
            S::Failed => RecordingStatus::Error,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct TimerInfoDto {
    pub id: String,
    #[serde(rename = "Type")]
    pub type_: String,
    pub server_id: String,
    pub channel_id: Option<Uuid>,
    pub program_id: Option<Uuid>,
    pub name: String,
    pub overview: Option<String>,
    pub start_date: DateTime<Utc>,
    pub end_date: DateTime<Utc>,
    pub service_name: String,
    pub priority: i32,
    pub pre_padding_seconds: i32,
    pub post_padding_seconds: i32,
    pub is_pre_padding_required: bool,
    pub is_post_padding_required: bool,
    pub status: RecordingStatus,
    pub series_timer_id: Option<String>,
    /// Only while the recording is in progress: the playable `Recording` item.
    /// A client that lists timers as "scheduled" (Moonfin ignores `Status`)
    /// can then open it, since a timer's own id isn't an item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program_info: Option<api::BaseItemDto>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SeriesTimerInfoDto {
    pub id: String,
    #[serde(rename = "Type")]
    pub type_: String,
    pub server_id: String,
    pub name: String,
    pub overview: Option<String>,
    pub service_name: String,
    pub priority: i32,
    pub record_any_time: bool,
    pub record_any_channel: bool,
    pub record_new_only: bool,
    pub skip_episodes_in_library: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CreateTimerRequest {
    pub program_id: Option<Uuid>,
    pub channel_id: Option<Uuid>,
    pub start_date: Option<DateTime<Utc>>,
    pub end_date: Option<DateTime<Utc>>,
    #[serde(default)]
    pub pre_padding_seconds: i32,
    #[serde(default)]
    pub post_padding_seconds: i32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CreateSeriesTimerRequest {
    pub program_id: Option<Uuid>,
    pub channel_id: Option<Uuid>,
    pub name: Option<String>,
    #[serde(default)]
    pub record_new_only: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addons::dispatcharr_dvr::{
        DispatcharrRecording, DispatcharrRecordingStatus as Status,
        DispatcharrSeriesRule,
    };
    use serde_json::{Value, json};

    const ADDON: Uuid = Uuid::from_u128(0xd15b);

    fn recording(id: i64, custom_properties: Value) -> DispatcharrRecording {
        serde_json::from_value(json!({
            "id": id,
            "channel": 42,
            "start_time": "2026-09-15T20:00:00Z",
            "end_time": "2026-09-15T21:00:00Z",
            "custom_properties": custom_properties,
        }))
        .unwrap()
    }

    fn rule(
        tvg: Option<&str>,
        title: Option<&str>,
        epg: Option<i64>,
    ) -> DispatcharrSeriesRule {
        DispatcharrSeriesRule {
            tvg_id: tvg.map(str::to_owned),
            mode: "all".into(),
            title: title.map(str::to_owned),
            description: None,
            epg_source_id: epg,
        }
    }

    // -- series timer ids ---------------------------------------------

    #[test]
    fn series_timer_id_is_the_percent_encoded_composite() {
        let id = series_timer_id(
            ADDON,
            &rule(Some("BBC1.uk"), Some("Match of the Day"), Some(3)),
        );
        assert_eq!(
            id,
            format!("{}%7CBBC1.uk%7CMatch%20of%20the%20Day%7C3", ADDON.simple())
        );
    }

    #[test]
    fn series_timer_id_round_trips() {
        let cases = [
            (Some("BBC1.uk"), Some("Match of the Day"), Some(3)),
            (Some("BBC1.uk"), Some("Match of the Day"), None),
            (None, Some("Any channel"), None),
            (Some("BBC1.uk"), None, None),
            (None, None, None),
            (Some("a b"), Some("Q&A: 50% off"), Some(12)),
        ];
        for (tvg, title, epg) in cases {
            let id = series_timer_id(ADDON, &rule(tvg, title, epg));
            let want = (
                Some(ADDON),
                tvg.map(str::to_owned),
                title.map(str::to_owned),
                epg,
            );
            assert_eq!(parse_series_timer_id(&id), Some(want), "{id}");
        }
    }

    #[test]
    fn series_timer_id_round_trips_a_title_containing_the_separator() {
        let id = series_timer_id(
            ADDON,
            &rule(Some("BBC1.uk"), Some("Cats | Dogs"), Some(3)),
        );
        let want = (
            Some(ADDON),
            Some("BBC1.uk".to_owned()),
            Some("Cats | Dogs".to_owned()),
            Some(3),
        );
        assert_eq!(parse_series_timer_id(&id), Some(want), "{id}");
    }

    #[test]
    fn parse_series_timer_id_accepts_an_already_decoded_id() {
        // axum percent-decodes path params before the handler sees them.
        assert_eq!(
            parse_series_timer_id(&format!(
                "{}|BBC1.uk|Match of the Day|3",
                ADDON.simple()
            )),
            Some((
                Some(ADDON),
                Some("BBC1.uk".into()),
                Some("Match of the Day".into()),
                Some(3)
            ))
        );
    }

    #[test]
    fn a_series_timer_id_from_before_instances_were_named_still_parses() {
        assert_eq!(
            parse_series_timer_id("BBC1.uk|Match of the Day|3"),
            Some((
                None,
                Some("BBC1.uk".into()),
                Some("Match of the Day".into()),
                Some(3)
            ))
        );
    }

    #[test]
    fn parse_series_timer_id_ignores_a_non_numeric_source() {
        assert_eq!(
            parse_series_timer_id(&format!("{}|BBC1.uk|MOTD|abc", ADDON.simple())),
            Some((
                Some(ADDON),
                Some("BBC1.uk".into()),
                Some("MOTD".into()),
                None
            ))
        );
    }

    // -- timer ids ------------------------------------------------------

    #[test]
    fn a_timer_id_names_the_instance_it_lives_on() {
        let id = TimerId {
            addon_id: ADDON,
            recording_id: 7,
        };
        assert_eq!(id.to_string(), format!("{}:7", ADDON.simple()));
        assert_eq!(
            id.to_string()
                .parse::<TimerId>(),
            Ok(id)
        );
        assert!(
            "7".parse::<TimerId>()
                .is_err(),
            "a bare number names no instance"
        );
        assert!(
            "nothex:7"
                .parse::<TimerId>()
                .is_err()
        );
    }

    #[test]
    fn parse_series_timer_id_rejects_invalid_utf8() {
        assert_eq!(parse_series_timer_id("%FF%FE"), None);
    }

    // -- conversions --------------------------------------------------

    #[test]
    fn channel_uuid_matches_the_synced_channel_row() {
        let ch: dispatcharr::DispatcharrChannel =
            serde_json::from_value(json!({ "id": 42, "uuid": "u", "name": "BBC One" }))
                .unwrap();
        assert_eq!(
            channel_uuid_of(ADDON, 42),
            dispatcharr::channel_to_media(&ch, ADDON, "s").id
        );
    }

    #[test]
    fn timer_from_recording_maps_fields() {
        let rec = recording(
            7,
            json!({ "status": "recording",
                    "program": { "title": "MOTD", "description": "Highlights" } }),
        );
        let t = timer_from_recording(ADDON, &rec);
        assert_eq!(t.id, format!("{}:7", ADDON.simple()));
        assert_eq!(t.type_, "Timer");
        assert_eq!(t.name, "MOTD");
        assert_eq!(
            t.overview
                .as_deref(),
            Some("Highlights")
        );
        assert_eq!(t.channel_id, Some(channel_uuid_of(ADDON, 42)));
        assert_eq!(t.start_date, rec.start_time);
        assert_eq!(t.end_date, rec.end_time);
        assert_eq!(t.service_name, "dispatcharr");
        assert_eq!(t.status, RecordingStatus::InProgress);
        assert_eq!(t.server_id, crate::common::server_id());
        assert_eq!(t.series_timer_id, None);
    }

    #[test]
    fn only_a_recording_in_progress_carries_its_item_as_program_info() {
        let running = recording(7, json!({ "status": "recording" }));
        let t = timer_from_recording(ADDON, &running);
        let item = t
            .program_info
            .expect("in-progress timer points at its recording");
        assert_eq!(
            item.id,
            Uuid::new_v5(&ADDON, b"recording:7"),
            "same id the synced Recording row gets"
        );
        for status in [json!({}), json!({ "status": "completed" })] {
            assert!(
                timer_from_recording(ADDON, &recording(7, status))
                    .program_info
                    .is_none()
            );
        }
        let v =
            serde_json::to_value(timer_from_recording(ADDON, &recording(7, json!({}))))
                .unwrap();
        assert!(
            v.get("ProgramInfo")
                .is_none(),
            "omitted when absent: {v}"
        );
    }

    #[tokio::test]
    async fn a_series_timer_is_found_by_the_id_its_own_listing_returns() {
        // Regression: `SeriesTimerInfoDto::id` is percent-encoded, but a
        // path segment reaches the handler already decoded by axum. Fetching
        // a series timer by an id copied straight from `GET
        // /livetv/seriestimers` must not 404.
        use crate::integration_test::new_test_server;

        let (_server, guard) = new_test_server()
            .await
            .unwrap();
        let ctx = &guard.0;
        let server = httpmock::MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/channels/series-rules/");
            then.status(200)
                .json_body(json!({
                    "rules": [{
                        "tvg_id": "BBC1.uk",
                        "mode": "all",
                        "title": "Match of the Day",
                        "epg_source_id": 3,
                    }],
                }));
        });
        register_dispatcharr(ctx, ADDON, &server.base_url()).await;

        let listed = DvrService::list_series_timers(ctx)
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        let id = &listed[0].id;
        assert!(
            id.contains("%7C"),
            "id should be percent-encoded like axum would receive it decoded: {id}"
        );

        let found = DvrService::get_series_timer(ctx, id)
            .await
            .unwrap()
            .expect("series timer found by its own listed id");
        assert_eq!(found.id, *id);

        assert!(
            DvrService::get_series_timer(ctx, "nope|nope|nope")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn recordings_are_listed_newest_first() {
        let item = |start: Option<&str>| api::BaseItemDto {
            start_date: start.map(str::to_owned),
            ..Default::default()
        };
        let mut items = vec![
            item(Some("2026-09-04T17:33:24.674911+00:00")),
            item(None),
            item(Some("2026-09-21T20:20:54.518446+00:00")),
            item(Some("2026-09-12T22:00:00+00:00")),
        ];
        newest_first(&mut items);
        let order: Vec<_> = items
            .iter()
            .map(|i| {
                i.start_date
                    .clone()
            })
            .collect();
        assert_eq!(
            order,
            vec![
                Some("2026-09-21T20:20:54.518446+00:00".to_string()),
                Some("2026-09-12T22:00:00+00:00".to_string()),
                Some("2026-09-04T17:33:24.674911+00:00".to_string()),
                None,
            ]
        );
    }

    #[tokio::test]
    async fn a_timer_id_resolves_from_its_number_or_its_recordings_uuid() {
        use crate::integration_test::new_test_server;

        let (_server, guard) = new_test_server()
            .await
            .unwrap();
        let ctx = &guard.0;
        let server = httpmock::MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/channels/recordings/");
            then.status(200)
                .json_body(json!([]));
        });
        register_dispatcharr(ctx, ADDON, &server.base_url()).await;
        let source = ADDON
            .simple()
            .to_string();

        let rec = recording(7, json!({ "status": "recording" }));
        let channel: dispatcharr::DispatcharrChannel =
            serde_json::from_value(json!({ "id": 42, "uuid": "u", "name": "BBC" }))
                .unwrap();
        db::Media::upsert(
            &ctx.db,
            &[dispatcharr::channel_to_media(&channel, ADDON, &source)],
        )
        .await
        .unwrap();
        let media = dispatcharr::recording_to_media(&rec, ADDON, &source);
        db::Media::upsert(&ctx.db, &[media.clone()])
            .await
            .unwrap();

        let want = TimerId {
            addon_id: ADDON,
            recording_id: 7,
        };
        assert_eq!(
            DvrService::resolve_timer_id(ctx, &want.to_string()).await,
            Some(want)
        );
        // A bare number is an id cached before timers named their instance.
        assert_eq!(DvrService::resolve_timer_id(ctx, "7").await, Some(want));
        assert_eq!(
            DvrService::resolve_timer_id(
                ctx,
                &media
                    .id
                    .to_string()
            )
            .await,
            Some(want)
        );
        assert_eq!(
            DvrService::resolve_timer_id(ctx, &Uuid::new_v4().to_string()).await,
            None
        );
        assert_eq!(DvrService::resolve_timer_id(ctx, "nope").await, None);
    }

    #[tokio::test]
    async fn a_timer_is_cancelled_on_the_instance_its_id_names() {
        use crate::integration_test::new_test_server;

        let (_server, guard) = new_test_server()
            .await
            .unwrap();
        let ctx = &guard.0;
        let addon_a = Uuid::from_u128(0xaaa);
        let addon_b = Uuid::from_u128(0xbbb);

        let server_a = httpmock::MockServer::start();
        let touched_a = server_a.mock(|when, then| {
            when.any_request();
            then.status(200)
                .json_body(json!([]));
        });
        let server_b = httpmock::MockServer::start();
        server_b.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/channels/recordings/7/");
            then.status(200)
                .json_body(rec_json(7, Some("completed")));
        });
        let delete_b = server_b.mock(|when, then| {
            when.method(httpmock::Method::DELETE)
                .path("/api/channels/recordings/7/");
            then.status(204);
        });
        register_dispatcharr(ctx, addon_a, &server_a.base_url()).await;
        register_dispatcharr(ctx, addon_b, &server_b.base_url()).await;

        let id = TimerId {
            addon_id: addon_b,
            recording_id: 7,
        };
        assert!(
            DvrService::delete_timer(ctx, id)
                .await
                .unwrap()
        );
        delete_b.assert();
        touched_a.assert_hits(0);
    }

    #[test]
    fn a_recording_is_found_by_its_derived_item_id_before_it_is_synced() {
        let recs = [
            recording(3, json!({})),
            recording(7, json!({ "status": "recording" })),
        ];
        let wanted = dispatcharr::recording_media_id(ADDON, 7);
        assert_eq!(recording_id_for_uuid(&recs, ADDON, wanted), Some(7));
        // The id depends on the addon, so another addon's recording never matches.
        let other = dispatcharr::recording_media_id(Uuid::from_u128(1), 7);
        assert_eq!(recording_id_for_uuid(&recs, ADDON, other), None);
        assert_eq!(recording_id_for_uuid(&[], ADDON, wanted), None);
    }

    #[test]
    fn timer_from_recording_names_unlabelled_recordings() {
        let t = timer_from_recording(ADDON, &recording(1, json!({})));
        assert_eq!(t.name, "Recording");
        assert_eq!(t.overview, None);
        assert_eq!(t.status, RecordingStatus::New);
    }

    #[test]
    fn timer_serialises_with_jellyfin_field_names() {
        let t = timer_from_recording(
            ADDON,
            &recording(1, json!({ "status": "completed" })),
        );
        let v = serde_json::to_value(t).unwrap();
        for key in [
            "Id",
            "Type",
            "ServerId",
            "ChannelId",
            "Name",
            "StartDate",
            "EndDate",
            "ServiceName",
            "PrePaddingSeconds",
            "IsPrePaddingRequired",
            "Status",
        ] {
            assert!(
                v.get(key)
                    .is_some(),
                "missing {key} in {v}"
            );
        }
        assert_eq!(v["Status"], "Completed");
    }

    #[test]
    fn series_timer_from_rule_maps_fields() {
        let mut r = rule(Some("BBC1.uk"), Some("MOTD"), Some(3));
        r.mode = "new".into();
        r.description = Some("desc".into());
        let s = series_timer_from_rule(ADDON, &r);
        assert_eq!(s.id, series_timer_id(ADDON, &r));
        assert_eq!(s.type_, "SeriesTimer");
        assert_eq!(s.name, "MOTD");
        assert_eq!(
            s.overview
                .as_deref(),
            Some("desc")
        );
        assert!(s.record_new_only);
        assert!(!s.record_any_channel);
        assert!(s.record_any_time);
    }

    #[test]
    fn series_timer_without_tvg_id_records_any_channel() {
        let s = series_timer_from_rule(ADDON, &rule(None, None, None));
        assert_eq!(s.name, "All programs");
        assert!(s.record_any_channel);
        assert!(!s.record_new_only, "mode \"all\" is not new-only");
    }

    #[test]
    fn recording_status_maps_to_jellyfin_states() {
        for (from, want) in [
            (Status::Scheduled, RecordingStatus::New),
            (Status::Recording, RecordingStatus::InProgress),
            (Status::Completed, RecordingStatus::Completed),
            (Status::Stopped, RecordingStatus::Cancelled),
            (Status::Interrupted, RecordingStatus::Cancelled),
            (Status::Failed, RecordingStatus::Error),
        ] {
            assert_eq!(RecordingStatus::from(from), want, "{from:?}");
        }
    }

    #[test]
    fn recording_status_serialises_pascal_case() {
        let s = |st| serde_json::to_value(st).unwrap();
        assert_eq!(s(RecordingStatus::InProgress), "InProgress");
        assert_eq!(s(RecordingStatus::New), "New");
        assert_eq!(s(RecordingStatus::Cancelled), "Cancelled");
    }

    #[test]
    fn create_requests_deserialise_jellyfin_bodies() {
        let t: CreateTimerRequest = serde_json::from_value(json!({
            "ChannelId": "00000000-0000-0000-0000-000000000001",
            "StartDate": "2026-10-20T03:00:00Z",
            "EndDate": "2026-10-20T03:10:00Z",
        }))
        .unwrap();
        assert_eq!(t.channel_id, Some(Uuid::from_u128(1)));
        assert_eq!(t.program_id, None);
        assert_eq!((t.pre_padding_seconds, t.post_padding_seconds), (0, 0));

        let padded: CreateTimerRequest = serde_json::from_value(json!({
            "ProgramId": "00000000-0000-0000-0000-000000000002",
            "PrePaddingSeconds": 60, "PostPaddingSeconds": 300,
        }))
        .unwrap();
        assert_eq!(
            (padded.pre_padding_seconds, padded.post_padding_seconds),
            (60, 300)
        );

        let s: CreateSeriesTimerRequest =
            serde_json::from_value(json!({ "Name": "MOTD", "RecordNewOnly": true }))
                .unwrap();
        assert!(s.record_new_only);
        assert_eq!(
            s.name
                .as_deref(),
            Some("MOTD")
        );
        let default: CreateSeriesTimerRequest =
            serde_json::from_value(json!({})).unwrap();
        assert!(!default.record_new_only);
    }

    // -- sync_recordings ---------------------------------------------

    fn rec_json(id: i64, status: Option<&str>) -> Value {
        let props = match status {
            Some(s) => {
                json!({ "status": s, "program": { "title": format!("Show {id}") } })
            }
            None => json!({}),
        };
        json!({
            "id": id, "channel": 42,
            "start_time": "2026-09-15T20:00:00Z",
            "end_time": "2026-09-15T21:00:00Z",
            "custom_properties": props,
        })
    }

    async fn seed_channel(ctx: &AppContext, addon: Uuid, source: &str) {
        let ch: dispatcharr::DispatcharrChannel =
            serde_json::from_value(json!({ "id": 42, "uuid": "u", "name": "BBC One" }))
                .unwrap();
        db::Media::upsert(
            &ctx.db,
            &vec![dispatcharr::channel_to_media(&ch, addon, source)],
        )
        .await
        .unwrap();
    }

    /// Registers `addon` as an enabled Dispatcharr instance pointed at
    /// `base_url`, so `configs()` can resolve it.
    async fn register_dispatcharr(ctx: &AppContext, addon: Uuid, base_url: &str) {
        let now = chrono::Utc::now().naive_utc();
        let row = crate::addons::Addon {
            id: addon,
            name: format!("dispatcharr-{addon}"),
            preset: crate::addons::AddonPresetRef {
                kind: "dispatcharr".into(),
                config: json!({ "base_url": base_url, "api_key": "k" }).into(),
            },
            resources: vec![],
            types: vec![],
            enabled: true,
            priority: 0,
            created_at: now,
            updated_at: now,
            system: false,
            is_default: true,
            http_redirect_stream: false,
            service_filter: vec![],
        };
        row.insert(&ctx.db)
            .await
            .unwrap();
        let mut runtimes: Vec<crate::addons::AddonRuntime> = ctx
            .addons
            .list()
            .iter()
            .cloned()
            .collect();
        runtimes.push(crate::addons::AddonRuntime {
            row,
            caps: Default::default(),
        });
        ctx.addons
            .replace_runtimes_for_test(runtimes);
    }

    async fn recording_ids(ctx: &AppContext) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = db::Media::get_by_filter(
            &ctx.db,
            &db::MediaFilter {
                kind: Some(vec![db::MediaKind::Recording]),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .records
        .into_iter()
        .map(|m| m.id)
        .collect();
        ids.sort();
        ids
    }

    async fn stream_count(ctx: &AppContext) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM media WHERE kind = 'stream'")
            .fetch_one(&ctx.db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn sync_recordings_skips_scheduled_and_prunes_only_its_own_addon() {
        use crate::integration_test::new_test_server;

        let (_server, guard) = new_test_server()
            .await
            .unwrap();
        let ctx = &guard.0;
        let other_addon = Uuid::from_u128(0x0dd);
        seed_channel(
            ctx,
            ADDON,
            &ADDON
                .simple()
                .to_string(),
        )
        .await;
        seed_channel(
            ctx,
            other_addon,
            &other_addon
                .simple()
                .to_string(),
        )
        .await;

        // A recording that belongs to a different Dispatcharr addon.
        let foreign = dispatcharr::recording_to_media(
            &recording(99, json!({ "status": "completed" })),
            other_addon,
            &other_addon
                .simple()
                .to_string(),
        );
        db::Media::upsert(&ctx.db, &vec![foreign.clone()])
            .await
            .unwrap();

        let server = httpmock::MockServer::start();
        let mut list = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/channels/recordings/")
                .header("X-API-Key", "k");
            then.status(200)
                .json_body(json!([
                    rec_json(1, None),
                    rec_json(2, Some("recording")),
                    rec_json(3, Some("completed")),
                    rec_json(4, Some("failed")),
                ]));
        });
        let cfg = DvrConfig {
            addon_id: ADDON,
            base_url: server.base_url(),
            api_key: "k".into(),
        };
        let rid = |n: i64| Uuid::new_v5(&ADDON, format!("recording:{n}").as_bytes());

        // Scheduled (id 1) has no file yet, so it is not synced as playable.
        assert_eq!(
            DvrService::sync_recordings(ctx, &cfg)
                .await
                .unwrap(),
            3
        );
        let mut want = vec![rid(2), rid(3), rid(4), foreign.id];
        want.sort();
        assert_eq!(recording_ids(ctx).await, want);
        assert_eq!(
            stream_count(ctx).await,
            3,
            "one playable child per recording"
        );

        // Dispatcharr drops recordings 2 and 4: they are pruned, with their
        // stream children, while the other addon's row is left alone.
        list.delete();
        server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/channels/recordings/");
            then.status(200)
                .json_body(json!([rec_json(3, Some("completed"))]));
        });
        assert_eq!(
            DvrService::sync_recordings(ctx, &cfg)
                .await
                .unwrap(),
            1
        );
        let mut want = vec![rid(3), foreign.id];
        want.sort();
        assert_eq!(recording_ids(ctx).await, want);
        assert_eq!(stream_count(ctx).await, 1);
    }

    #[tokio::test]
    async fn sync_recordings_with_none_left_prunes_everything_for_the_addon() {
        use crate::integration_test::new_test_server;

        let (_server, guard) = new_test_server()
            .await
            .unwrap();
        let ctx = &guard.0;
        seed_channel(
            ctx,
            ADDON,
            &ADDON
                .simple()
                .to_string(),
        )
        .await;

        let server = httpmock::MockServer::start();
        let mut list = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/channels/recordings/");
            then.status(200)
                .json_body(json!([rec_json(3, Some("completed"))]));
        });
        let cfg = DvrConfig {
            addon_id: ADDON,
            base_url: server.base_url(),
            api_key: "k".into(),
        };
        assert_eq!(
            DvrService::sync_recordings(ctx, &cfg)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            recording_ids(ctx)
                .await
                .len(),
            1
        );

        list.delete();
        server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/channels/recordings/");
            then.status(200)
                .json_body(json!([]));
        });
        assert_eq!(
            DvrService::sync_recordings(ctx, &cfg)
                .await
                .unwrap(),
            0
        );
        assert!(
            recording_ids(ctx)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn sync_recordings_reports_a_dispatcharr_failure() {
        use crate::integration_test::new_test_server;

        let (_server, guard) = new_test_server()
            .await
            .unwrap();
        let server = httpmock::MockServer::start();
        server.mock(|when, then| {
            when.any_request();
            then.status(500);
        });
        let cfg = DvrConfig {
            addon_id: ADDON,
            base_url: server.base_url(),
            api_key: "k".into(),
        };
        assert!(
            DvrService::sync_recordings(&guard.0, &cfg)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn deleting_a_recording_only_ever_reaches_its_own_instance() {
        use crate::integration_test::new_test_server;

        let (_server, guard) = new_test_server()
            .await
            .unwrap();
        let ctx = &guard.0;
        let addon_a = Uuid::from_u128(0xaaa);
        let addon_b = Uuid::from_u128(0xbbb);

        // Both instances happen to have a recording numbered 7.
        let server_a = httpmock::MockServer::start();
        let delete_a = server_a.mock(|when, then| {
            when.method(httpmock::Method::DELETE);
            then.status(204);
        });
        let server_b = httpmock::MockServer::start();
        let delete_b = server_b.mock(|when, then| {
            when.method(httpmock::Method::DELETE)
                .path("/api/channels/recordings/7/");
            then.status(204);
        });
        register_dispatcharr(ctx, addon_a, &server_a.base_url()).await;
        register_dispatcharr(ctx, addon_b, &server_b.base_url()).await;

        seed_channel(
            ctx,
            addon_b,
            &addon_b
                .simple()
                .to_string(),
        )
        .await;
        let row = dispatcharr::recording_to_media(
            &recording(7, json!({ "status": "completed" })),
            addon_b,
            &addon_b
                .simple()
                .to_string(),
        );
        db::Media::upsert(&ctx.db, &vec![row.clone()])
            .await
            .unwrap();

        assert!(
            DvrService::delete_recording(ctx, row.id)
                .await
                .unwrap()
        );
        delete_b.assert();
        delete_a.assert_hits(0);
        assert!(
            recording_ids(ctx)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_recording_whose_instance_is_gone_is_not_deleted_elsewhere() {
        use crate::integration_test::new_test_server;

        let (_server, guard) = new_test_server()
            .await
            .unwrap();
        let ctx = &guard.0;
        let addon_a = Uuid::from_u128(0xaaa);
        let removed = Uuid::from_u128(0xdead);

        let server_a = httpmock::MockServer::start();
        let delete_a = server_a.mock(|when, then| {
            when.method(httpmock::Method::DELETE);
            then.status(204);
        });
        register_dispatcharr(ctx, addon_a, &server_a.base_url()).await;

        seed_channel(
            ctx,
            removed,
            &removed
                .simple()
                .to_string(),
        )
        .await;
        let row = dispatcharr::recording_to_media(
            &recording(7, json!({ "status": "completed" })),
            removed,
            &removed
                .simple()
                .to_string(),
        );
        db::Media::upsert(&ctx.db, &vec![row.clone()])
            .await
            .unwrap();

        // The local row still goes, but no other instance is asked to drop
        // whatever its own recording 7 happens to be.
        assert!(
            DvrService::delete_recording(ctx, row.id)
                .await
                .unwrap()
        );
        delete_a.assert_hits(0);
    }

    #[tokio::test]
    async fn sync_recordings_imports_the_channels_it_needs_as_parents() {
        use crate::integration_test::new_test_server;

        let (_server, guard) = new_test_server()
            .await
            .unwrap();
        let ctx = &guard.0;

        // No channel import has run — the state a freshly configured addon
        // is in when a client opens Live TV recordings.
        let server = httpmock::MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/channels/recordings/");
            then.status(200)
                .json_body(json!([rec_json(3, Some("completed"))]));
        });
        let channels = server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/channels/channels/");
            then.status(200)
                .json_body(json!([{ "id": 42, "uuid": "u", "name": "BBC One" }]));
        });
        let cfg = DvrConfig {
            addon_id: ADDON,
            base_url: server.base_url(),
            api_key: "k".into(),
        };

        assert_eq!(
            DvrService::sync_recordings(ctx, &cfg)
                .await
                .unwrap(),
            1
        );
        channels.assert();
        assert_eq!(
            recording_ids(ctx).await,
            vec![Uuid::new_v5(&ADDON, b"recording:3")]
        );

        // The parent is there now, so a second pass must not re-fetch it.
        assert_eq!(
            DvrService::sync_recordings(ctx, &cfg)
                .await
                .unwrap(),
            1
        );
        channels.assert_hits(1);
    }

    #[tokio::test]
    async fn a_recording_on_a_vanished_channel_does_not_sink_the_whole_sync() {
        use crate::integration_test::new_test_server;

        let (_server, guard) = new_test_server()
            .await
            .unwrap();
        let ctx = &guard.0;

        let server = httpmock::MockServer::start();
        server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/channels/recordings/");
            then.status(200)
                .json_body(json!([
                    rec_json(3, Some("completed")),
                    // channel 77 is not in the channel list below
                    json!({
                        "id": 4, "channel": 77,
                        "start_time": "2026-09-15T20:00:00Z",
                        "end_time": "2026-09-15T21:00:00Z",
                        "custom_properties": { "status": "completed" },
                    }),
                ]));
        });
        server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/api/channels/channels/");
            then.status(200)
                .json_body(json!([{ "id": 42, "uuid": "u", "name": "BBC One" }]));
        });
        let cfg = DvrConfig {
            addon_id: ADDON,
            base_url: server.base_url(),
            api_key: "k".into(),
        };

        assert_eq!(
            DvrService::sync_recordings(ctx, &cfg)
                .await
                .unwrap(),
            1,
            "the orphan is skipped, the other still syncs"
        );
        assert_eq!(
            recording_ids(ctx).await,
            vec![Uuid::new_v5(&ADDON, b"recording:3")]
        );
    }
}
