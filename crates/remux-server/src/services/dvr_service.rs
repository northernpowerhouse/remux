//! Jellyfin DVR (Timers / SeriesTimers / Recordings) backed by Dispatcharr.
//!
//! Timers and SeriesTimers are pure live forwarding — nothing persisted,
//! every call re-asks Dispatcharr for current state, since DVR writes are
//! rare human-triggered actions with no staleness tradeoff worth a cache.
//! Recordings are the one exception: they're synced into the `media` table
//! as real `MediaKind::Recording` rows (`RefreshDispatcharrLiveTvTask`,
//! `addons::dispatcharr::recording_to_media`/`recording_stream_to_media`),
//! because playback goes through the generic `/items/{id}` ->
//! `/items/{id}/playbackinfo` -> `/videos/{id}/stream` pipeline, and that
//! only resolves real rows — the same reason channel playback works
//! (`TvChannel` is a real row too). Playback still needs one live
//! Dispatcharr call at request time (`recording_playback_target`), since
//! `file_url` flips from an HLS playlist to a plain file as a recording
//! finishes and the synced row isn't refreshed that often.
//!
//! Id scheme: Dispatcharr recording ids are plain integers (used directly as
//! `TimerInfoDto.Id`, which Jellyfin's own spec types as a string, not a
//! Guid). Series rules have no id at all, so `SeriesTimerInfoDto.Id` is a
//! percent-encoded `tvg_id|title|epg_source_id` composite that delete parses
//! back apart. Channel resolution (Jellyfin `ChannelId` -> Dispatcharr
//! integer channel id, needed by `create_timer`/`create_series_timer`) goes
//! through a live `fetch_channels()` call and matches the recomputed
//! `Uuid::new_v5` hash — same trick `channel_to_media` uses to derive it.

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

pub struct DvrService;

impl DvrService {
    /// First enabled `dispatcharr`-kind addon instance. Jellyfin's DVR API
    /// has no per-provider concept to route by, so with more than one
    /// configured, the first one found wins.
    pub async fn active_config(ctx: &AppContext) -> Option<DvrConfig> {
        let runtimes = ctx
            .addons
            .list();
        let runtime = runtimes
            .iter()
            .find(|r| {
                r.row
                    .enabled
                    && r.row
                        .preset
                        .kind
                        == "dispatcharr"
            })?;
        let config = runtime
            .row
            .preset
            .config
            .expose();
        let base_url = config["base_url"]
            .as_str()
            .filter(|s| !s.is_empty())?
            .trim_end_matches('/')
            .to_string();
        let api_key = config["api_key"]
            .as_str()
            .filter(|s| !s.is_empty())?
            .to_string();
        Some(DvrConfig {
            addon_id: runtime
                .row
                .id,
            base_url,
            api_key,
        })
    }

    async fn resolve_channel_int(
        cfg: &DvrConfig,
        channel_uuid: Uuid,
    ) -> Result<Option<i64>> {
        let client = reqwest::Client::new();
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
        let client = reqwest::Client::new();
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
        let Some(cfg) = Self::active_config(ctx).await else {
            return Ok(vec![]);
        };
        let client = reqwest::Client::new();
        let recordings =
            dispatcharr_dvr::list_recordings(&client, &cfg.base_url, &cfg.api_key)
                .await?;
        let mut timers: Vec<TimerInfoDto> = recordings
            .iter()
            .filter(|r| {
                matches!(
                    r.status(),
                    dispatcharr_dvr::DispatcharrRecordingStatus::Scheduled
                        | dispatcharr_dvr::DispatcharrRecordingStatus::Recording
                )
            })
            .map(|r| timer_from_recording(cfg.addon_id, r))
            .collect();
        timers.sort_by_key(|t| t.start_date);
        Ok(timers)
    }

    pub async fn get_timer(ctx: &AppContext, id: i64) -> Result<Option<TimerInfoDto>> {
        let Some(cfg) = Self::active_config(ctx).await else {
            return Ok(None);
        };
        let client = reqwest::Client::new();
        match dispatcharr_dvr::get_recording(&client, &cfg.base_url, &cfg.api_key, id)
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
        let cfg = Self::active_config(ctx)
            .await
            .context("no Dispatcharr DVR configured")?;

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

        let channel_id = Self::resolve_channel_int(&cfg, channel_uuid)
            .await?
            .context("channel not found on Dispatcharr")?;
        let rec = dispatcharr_dvr::schedule_recording(
            &reqwest::Client::new(),
            &cfg.base_url,
            &cfg.api_key,
            channel_id,
            start,
            end,
        )
        .await?;
        Ok(timer_from_recording(cfg.addon_id, &rec))
    }

    /// Cancels the timer: stops it if it's currently recording (keeping the
    /// partial file), otherwise deletes the not-yet-started scheduled entry.
    pub async fn delete_timer(ctx: &AppContext, id: i64) -> Result<bool> {
        let Some(cfg) = Self::active_config(ctx).await else {
            return Ok(false);
        };
        let client = reqwest::Client::new();
        let Ok(rec) =
            dispatcharr_dvr::get_recording(&client, &cfg.base_url, &cfg.api_key, id)
                .await
        else {
            return Ok(false);
        };
        if rec.status() == dispatcharr_dvr::DispatcharrRecordingStatus::Recording {
            dispatcharr_dvr::stop_recording(&client, &cfg.base_url, &cfg.api_key, id)
                .await?;
        } else {
            dispatcharr_dvr::delete_recording(&client, &cfg.base_url, &cfg.api_key, id)
                .await?;
        }
        Ok(true)
    }

    // -- SeriesTimers ----------------------------------------------------

    pub async fn list_series_timers(
        ctx: &AppContext,
    ) -> Result<Vec<SeriesTimerInfoDto>> {
        let Some(cfg) = Self::active_config(ctx).await else {
            return Ok(vec![]);
        };
        let client = reqwest::Client::new();
        let rules =
            dispatcharr_dvr::list_series_rules(&client, &cfg.base_url, &cfg.api_key)
                .await?;
        Ok(rules
            .iter()
            .map(series_timer_from_rule)
            .collect())
    }

    pub async fn create_series_timer(
        ctx: &AppContext,
        req: CreateSeriesTimerRequest,
    ) -> Result<SeriesTimerInfoDto> {
        let cfg = Self::active_config(ctx)
            .await
            .context("no Dispatcharr DVR configured")?;

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
            &reqwest::Client::new(),
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
        Ok(series_timer_from_rule(created))
    }

    pub async fn delete_series_timer(ctx: &AppContext, id: &str) -> Result<bool> {
        let Some(cfg) = Self::active_config(ctx).await else {
            return Ok(false);
        };
        let Some((tvg_id, title, epg_source_id)) = parse_series_timer_id(id) else {
            return Ok(false);
        };
        dispatcharr_dvr::delete_series_rule(
            &reqwest::Client::new(),
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

    /// Fetches current recordings from Dispatcharr and upserts/prunes the
    /// synced `Recording` rows (+ their `Stream` children) for one addon.
    /// Shared by the periodic task and every on-access read below — the
    /// task calls this once per configured Dispatcharr addon; on-access
    /// reads call it once for `active_config()`'s single addon.
    pub async fn sync_recordings(ctx: &AppContext, cfg: &DvrConfig) -> Result<usize> {
        let client = reqwest::Client::new();
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
                    &cfg.base_url,
                    &cfg.api_key,
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
        if let Some(cfg) = Self::active_config(ctx).await {
            if let Err(e) = Self::sync_recordings(ctx, &cfg).await {
                tracing::warn!(error = %e, "failed to refresh recordings from Dispatcharr, serving last-synced state");
            }
        }
        let mut result = db::Media::get_by_filter(
            &ctx.db,
            &db::MediaFilter {
                kind: Some(vec![db::MediaKind::Recording]),
                sort_by: vec![api::ItemSortBy::StartDate],
                sort_order: vec![api::SortOrder::Descending],
                ..Default::default()
            },
        )
        .await?;
        db::Media::attach_streams(&ctx.db, &mut result.records).await?;
        Ok(result
            .records
            .into_iter()
            .map(|m| api::db_media_to_item(m, false))
            .collect())
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
            // giving up, rather than always paying the round trip.
            if let Some(cfg) = Self::active_config(ctx).await {
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
            Self::active_config(ctx).await,
        ) {
            dispatcharr_dvr::delete_recording(
                &reqwest::Client::new(),
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
        let Some(cfg) = Self::active_config(ctx).await else {
            return Ok(None);
        };
        let rec = dispatcharr_dvr::get_recording(
            &reqwest::Client::new(),
            &cfg.base_url,
            &cfg.api_key,
            dispatcharr_id,
        )
        .await?;
        Ok(Some((cfg, rec)))
    }
}

fn channel_uuid_of(addon_id: Uuid, channel_id: i64) -> Uuid {
    Uuid::new_v5(&addon_id, format!("channel:{channel_id}").as_bytes())
}

fn timer_from_recording(
    addon_id: Uuid,
    rec: &dispatcharr_dvr::DispatcharrRecording,
) -> TimerInfoDto {
    TimerInfoDto {
        id: rec
            .id
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
    }
}

fn series_timer_id(rule: &dispatcharr_dvr::DispatcharrSeriesRule) -> String {
    let raw = format!(
        "{}|{}|{}",
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

fn parse_series_timer_id(
    id: &str,
) -> Option<(Option<String>, Option<String>, Option<i64>)> {
    let decoded = urlencoding::decode(id).ok()?;
    let mut parts = decoded.splitn(3, '|');
    let tvg_id = parts
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let title = parts
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let epg_source_id = parts
        .next()
        .filter(|s| !s.is_empty())
        .and_then(|s| {
            s.parse()
                .ok()
        });
    Some((tvg_id, title, epg_source_id))
}

fn series_timer_from_rule(
    rule: &dispatcharr_dvr::DispatcharrSeriesRule,
) -> SeriesTimerInfoDto {
    SeriesTimerInfoDto {
        id: series_timer_id(rule),
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
