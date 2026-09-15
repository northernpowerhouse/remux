use anyhow::Result;
use async_trait::async_trait;
use std::{collections::HashMap, sync::Arc};
use tracing::{debug, error, info, warn};

use super::{ProgressReporter, Task, TaskCategory, TaskService};
use crate::{
    AppContext,
    addons::dispatcharr::{
        channel_to_media, fetch_channel_streams, fetch_channels, fetch_epg_data,
        fetch_epg_grid, stream_to_media,
    },
    db,
};

/// Syncs everything Dispatcharr-sourced that `RefreshIptvTask`'s generic
/// pipeline can't handle correctly for the `dispatcharr` addon — this task is
/// self-sufficient and does not depend on `RefreshIptvTask` having run first:
///
/// - **Channel rows**: also built and upserted here via the same
///   `channel_to_media()` the addon's `CatalogAddon::catalog_stream()` uses,
///   so this task works standalone. (`RefreshIptvTask` will *also* upsert the
///   same deterministically-id'd rows if it runs — harmless, idempotent.)
///   This has to happen before the two steps below, which insert children
///   referencing these rows by `parent_id` — a SQLite foreign-key violation
///   otherwise (confirmed live: the first version of this task assumed the
///   channel rows already existed and failed on exactly this).
/// - **Channel versions**: `import_catalog_items` only persists top-level
///   content kinds and drops `Stream`-kind children, so versions have to be
///   attached through the same direct `db::Media::upsert` +
///   `streams_refreshed_at` mechanism `AddonService::refresh_streams` already
///   uses for Movie/Episode — just driven from a sync pass instead of a
///   per-request dynamic dispatch.
/// - **EPG**: the generic per-addon EPG loop expects an unauthenticated XMLTV
///   URL (`config["epg_url"]`). Dispatcharr's `/api/*` endpoints all require
///   an `X-API-Key` header (confirmed live — everything 401s without one),
///   and its *unauthenticated* `/output/epg` XMLTV export uses a re-numbered
///   export-local `<channel id>` that does not correspond to any `tvg_id` on
///   our channel rows. So EPG is pulled from the authenticated
///   `/api/epg/grid/` JSON endpoint instead, matched via each channel's
///   `epg_data_id -> EPGData.tvg_id` (the authoritative assigned-guide link —
///   not the channel's own `tvg_id` field, which is only a copy and can go
///   stale) — mirroring `iptv::stream_import_epg`'s upsert/prune shape.
pub struct RefreshDispatcharrLiveTvTask;

#[async_trait]
impl Task for RefreshDispatcharrLiveTvTask {
    fn key(&self) -> &str {
        "RefreshDispatcharrLiveTv"
    }
    fn name(&self) -> &str {
        "Refresh Dispatcharr Channel Versions & EPG"
    }
    fn description(&self) -> &str {
        "Attaches every Dispatcharr stream assigned to a channel as a selectable version of \
         that channel (in Dispatcharr's own priority order) and imports its programme guide, \
         both via Dispatcharr's authenticated API."
    }
    fn short_description(&self) -> &str {
        "Syncs per-channel stream versions and EPG from Dispatcharr"
    }
    fn category(&self) -> TaskCategory {
        TaskCategory::LiveTv
    }

    async fn run(
        &self,
        ctx: AppContext,
        _tasks: Arc<TaskService>,
        progress: ProgressReporter,
    ) -> Result<()> {
        let runtimes = ctx
            .addons
            .catalogs_for_kinds(&ctx, &[db::MediaKind::TvChannel])
            .await;

        let dispatcharr_runtimes: Vec<_> = runtimes
            .iter()
            .filter(|(runtime, _)| {
                runtime
                    .row
                    .preset
                    .kind
                    == "dispatcharr"
            })
            .collect();

        let client = reqwest::Client::new();

        for (idx, (runtime, _)) in dispatcharr_runtimes
            .iter()
            .enumerate()
        {
            progress.report(
                idx,
                dispatcharr_runtimes
                    .len()
                    .max(1),
            );

            let addon_id = runtime
                .row
                .id;
            let config = runtime
                .row
                .preset
                .config
                .expose();
            let base_url = config["base_url"]
                .as_str()
                .unwrap_or("")
                .trim_end_matches('/')
                .to_string();
            let token = config["api_key"]
                .as_str()
                .unwrap_or("")
                .to_string();
            if base_url.is_empty() || token.is_empty() {
                continue;
            }

            let channels = match fetch_channels(&client, &base_url, &token).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(addon = %addon_id, error = %e, "failed to fetch Dispatcharr channels");
                    continue;
                }
            };

            debug!(addon = %addon_id, channels = channels.len(), "syncing Dispatcharr channel versions");

            let source_id = addon_id
                .simple()
                .to_string();
            let channel_media: Vec<db::Media> = channels
                .iter()
                .map(|ch| channel_to_media(ch, addon_id, &source_id))
                .collect();
            if let Err(e) = db::Media::upsert(&ctx.db, &channel_media).await {
                error!(addon = %addon_id, error = %e, "failed to upsert Dispatcharr channels, skipping versions/EPG for this addon");
                continue;
            }

            // `epg_data_id -> tvg_id`, so EPG matching goes through each
            // channel's authoritative assigned guide data rather than its own
            // `tvg_id` field, which is only a copy and can go stale.
            let epg_data_map: HashMap<i64, String> = match fetch_epg_data(
                &client, &base_url, &token,
            )
            .await
            {
                Ok(rows) => rows
                    .into_iter()
                    .filter_map(|d| {
                        d.tvg_id
                            .map(|t| (d.id, t))
                    })
                    .collect(),
                Err(e) => {
                    warn!(addon = %addon_id, error = %e, "failed to fetch Dispatcharr EPG data, EPG import will be skipped");
                    HashMap::new()
                }
            };

            // Captured once for this whole sync pass: every synced `Stream`
            // row's created_at/updated_at, and the parent's
            // `streams_refreshed_at` marker, must share this exact value so
            // `.streams()`'s freshness filter (`updated_at >= refreshed`)
            // includes what was just written instead of hiding it.
            let now = chrono::Utc::now().naive_utc();
            let mut all_sources: Vec<db::Media> = Vec::new();
            let mut refreshed_channel_ids: Vec<uuid::Uuid> = Vec::new();
            // Multiple channels can legitimately share one `tvg_id` (e.g. SD/HD
            // variants pointing at the same guide source) — each needs its own
            // program rows, so this maps to *all* matching channels, not just
            // the last one seen (confirmed live: 113 of 981 EPG-assigned
            // channels shared a `tvg_id` with another channel).
            let mut tvg_map: HashMap<String, Vec<uuid::Uuid>> = HashMap::new();

            for ch in &channels {
                let channel_media_id = uuid::Uuid::new_v5(
                    &addon_id,
                    format!("channel:{}", ch.id).as_bytes(),
                );
                if let Some(tvg_id) = ch
                    .epg_data_id
                    .and_then(|id| epg_data_map.get(&id))
                {
                    tvg_map
                        .entry(tvg_id.clone())
                        .or_default()
                        .push(channel_media_id);
                }

                let streams = match fetch_channel_streams(
                    &client, &base_url, &token, ch.id,
                )
                .await
                {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(addon = %addon_id, channel = ch.id, error = %e, "failed to fetch channel streams");
                        continue;
                    }
                };
                if streams.is_empty() {
                    continue;
                }

                for (i, stream) in streams
                    .iter()
                    .enumerate()
                {
                    // The default/first version plays via Dispatcharr's own
                    // channel-level proxy (its own failover selection);
                    // every other version pins a specific stream by hash —
                    // see `/proxy/ts/stream/<id>`'s dual UUID/stream_hash
                    // resolution in Dispatcharr's `get_stream_object()`.
                    let playback_id = if i == 0 {
                        ch.uuid
                            .clone()
                    } else {
                        stream
                            .stream_hash
                            .clone()
                            .unwrap_or_else(|| {
                                stream
                                    .id
                                    .to_string()
                            })
                    };
                    all_sources.push(stream_to_media(
                        stream,
                        channel_media_id,
                        i as i64,
                        &playback_id,
                        &base_url,
                        &token,
                        now,
                    ));
                }
                refreshed_channel_ids.push(channel_media_id);
            }

            if !all_sources.is_empty() {
                if let Err(e) = db::Media::upsert(&ctx.db, &all_sources).await {
                    error!(addon = %addon_id, error = %e, "failed to upsert Dispatcharr channel versions");
                } else {
                    for channel_id in &refreshed_channel_ids {
                        let _ = sqlx::query(
                            "UPDATE media SET streams_refreshed_at = ? WHERE id = ?",
                        )
                        .bind(now)
                        .bind(channel_id)
                        .execute(&ctx.db)
                        .await;
                        // Drop versions no longer assigned to this channel in
                        // Dispatcharr — same 1-day grace window (not an
                        // immediate cutoff) `AddonService::refresh_streams`
                        // uses, so an in-flight playback session on a
                        // just-removed version isn't yanked out from under it.
                        let _ = sqlx::query(
                            "DELETE FROM media WHERE kind = 'stream' AND parent_id = ? \
                             AND updated_at < datetime('now', '-1 days')",
                        )
                        .bind(channel_id)
                        .execute(&ctx.db)
                        .await;
                    }
                    info!(
                        addon = %addon_id,
                        channels = refreshed_channel_ids.len(),
                        versions = all_sources.len(),
                        "Dispatcharr channel versions synced"
                    );
                }
            }

            if tvg_map.is_empty() {
                continue;
            }

            let programs = match fetch_epg_grid(&client, &base_url, &token).await {
                Ok(p) => p,
                Err(e) => {
                    warn!(addon = %addon_id, error = %e, "failed to fetch Dispatcharr EPG grid");
                    continue;
                }
            };

            let import_start = chrono::Utc::now().naive_utc();
            let mut batch: Vec<db::Media> = Vec::with_capacity(500);
            let mut program_total = 0usize;
            for prog in &programs {
                let Some(channel_ids) = tvg_map.get(&prog.tvg_id) else {
                    continue;
                };
                let live_start = prog
                    .start_time
                    .naive_utc();
                let live_end = prog
                    .end_time
                    .naive_utc();
                for &channel_id in channel_ids {
                    let prog_id = uuid::Uuid::new_v5(
                        &channel_id,
                        format!("{}{}", prog.start_time, prog.title).as_bytes(),
                    );
                    batch.push(db::Media {
                        id: prog_id,
                        title: prog
                            .title
                            .clone(),
                        kind: db::MediaKind::TvProgram,
                        parent_id: Some(channel_id),
                        description: prog
                            .description
                            .clone()
                            .or_else(|| {
                                prog.sub_title
                                    .clone()
                            }),
                        live_start: Some(live_start),
                        live_end: Some(live_end),
                        ..Default::default()
                    });
                    program_total += 1;
                    if batch.len() >= 500 {
                        if let Err(e) = db::Media::upsert(&ctx.db, &batch).await {
                            warn!(addon = %addon_id, error = %e, "failed to upsert EPG batch");
                        }
                        batch.clear();
                    }
                }
            }
            if !batch.is_empty() {
                if let Err(e) = db::Media::upsert(&ctx.db, &batch).await {
                    warn!(addon = %addon_id, error = %e, "failed to upsert EPG batch");
                }
            }

            // Prune programs for these channels not re-imported this run,
            // and anything that ended over a day ago — same convention as
            // `iptv::stream_import_epg`.
            let channel_ids: Vec<uuid::Uuid> = tvg_map
                .values()
                .flatten()
                .copied()
                .collect();
            for chunk in channel_ids.chunks(200) {
                let mut qb = sqlx::QueryBuilder::new(
                    "DELETE FROM media WHERE kind = 'tv_program' AND updated_at < ",
                );
                qb.push_bind(import_start);
                qb.push(" AND parent_id IN (");
                let mut sep = qb.separated(", ");
                for id in chunk {
                    sep.push_bind(id);
                }
                qb.push(")");
                let _ = qb
                    .build()
                    .execute(&ctx.db)
                    .await;

                let mut qb2 = sqlx::QueryBuilder::new(
                    "DELETE FROM media WHERE kind = 'tv_program' \
                     AND live_end < datetime('now', '-1 day') AND parent_id IN (",
                );
                let mut sep2 = qb2.separated(", ");
                for id in chunk {
                    sep2.push_bind(id);
                }
                qb2.push(")");
                let _ = qb2
                    .build()
                    .execute(&ctx.db)
                    .await;
            }

            info!(addon = %addon_id, programs = program_total, "Dispatcharr EPG synced");
        }

        progress.set(100.0);
        Ok(())
    }
}
