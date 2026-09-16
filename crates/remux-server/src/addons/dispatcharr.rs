use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use remux_sdks::stremio::MediaType as StremioMediaType;
use serde::Deserialize;
use std::{pin::Pin, sync::Arc};
use tracing::debug;
use uuid::Uuid;

use super::{
    AddonCapabilities, AddonKind, AddonMetadata, AddonOption, AddonOptionType,
    AddonPreset, AddonPresetRegistration, CatalogAddon, CatalogInfo, MediaKind,
    ResourceType, StreamAddon,
};
use crate::{AppContext, db};

// ---------------------------------------------------------------------------
// DispatcharrPreset
// ---------------------------------------------------------------------------

pub struct DispatcharrPreset;

impl AddonPreset for DispatcharrPreset {
    fn id(&self) -> &'static str {
        "dispatcharr"
    }

    fn metadata(&self) -> AddonMetadata {
        AddonMetadata {
            id: "dispatcharr".to_string(),
            display_name: "Dispatcharr".to_string(),
            description: "Import live TV channels from a Dispatcharr instance, with every \
                assigned stream shown as a selectable version."
                .to_string(),
            icon: None,
            supported_resources: vec![
                AddonMetadata::simple_resource(ResourceType::Stream),
                AddonMetadata::simple_resource(ResourceType::Catalog),
            ],
            supported_types: vec![MediaKind::TvChannel],
            supported_resources_user: vec![],
            supported_types_user: vec![],
            options: vec![
                AddonOption {
                    id: "base_url".to_string(),
                    name: "Base URL".to_string(),
                    description: Some(
                        "Base URL of the Dispatcharr instance (e.g. http://192.168.1.180:9191)."
                            .to_string(),
                    ),
                    required: true,
                    default: None,
                    kind: AddonOptionType::Url,
                },
                AddonOption {
                    id: "api_key".to_string(),
                    name: "API Key".to_string(),
                    description: Some(
                        "A Dispatcharr API key (Dispatcharr admin UI → API keys), sent as an \
                         X-API-Key header. Dedicated key recommended over reusing another \
                         integration's."
                            .to_string(),
                    ),
                    required: true,
                    default: None,
                    kind: AddonOptionType::Password,
                },
            ],
        }
    }

    fn from_cfg(
        &self,
        addon_id: Uuid,
        cfg: &serde_json::Value,
        _config: &crate::Config,
    ) -> Result<AddonCapabilities> {
        let base_url = cfg["base_url"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("dispatcharr: base_url is required"))?
            .trim_end_matches('/')
            .to_string();
        let api_key = cfg["api_key"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("dispatcharr: api_key is required"))?
            .to_string();

        let addon = Arc::new(DispatcharrAddon {
            addon_id,
            base_url,
            api_key,
        });
        Ok(AddonCapabilities {
            kind: Some(addon.clone()),
            catalog: Some(addon.clone()),
            stream: Some(addon),
            ..Default::default()
        })
    }
}

inventory::submit! {
    AddonPresetRegistration(|| Box::new(DispatcharrPreset))
}

// ---------------------------------------------------------------------------
// Dispatcharr API client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DispatcharrChannel {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub channel_number: Option<f64>,
    pub tvg_id: Option<String>,
    /// FK to `EPGData` — the authoritative link to this channel's assigned
    /// guide data. More reliable than this row's own `tvg_id` field, which
    /// is a copy that can go stale; always resolve EPG through this instead.
    #[serde(default)]
    pub epg_data_id: Option<i64>,
    #[serde(default)]
    pub logo_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DispatcharrStream {
    pub id: i64,
    pub name: String,
    pub stream_hash: Option<String>,
    #[serde(default)]
    pub stream_stats: Option<serde_json::Value>,
}

/// Fetch every Dispatcharr channel.
///
/// Called with no query params, this returns a plain JSON array of *every*
/// channel, not DRF's `{count,next,previous,results}` envelope — that
/// wrapper only appears when a `page_size` param is explicitly passed. Same
/// for `/api/epg/epgdata/` (`fetch_epg_data`) and the per-channel
/// `/streams/` endpoint (`fetch_channel_streams`) — neither paginates by
/// default either.
pub(crate) async fn fetch_channels(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> Result<Vec<DispatcharrChannel>> {
    let resp = client
        .get(format!("{base_url}/api/channels/channels/"))
        .header("X-API-Key", token)
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<DispatcharrChannel>>()
        .await?;
    Ok(resp)
}

/// Fetch the full `Stream` objects assigned to one channel, in Dispatcharr's own
/// priority order (the order Dispatcharr itself returns them in — array order
/// on `Channel.streams` is the only priority signal Dispatcharr has).
pub(crate) async fn fetch_channel_streams(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    channel_id: i64,
) -> Result<Vec<DispatcharrStream>> {
    let resp = client
        .get(format!(
            "{base_url}/api/channels/channels/{channel_id}/streams/"
        ))
        .header("X-API-Key", token)
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<DispatcharrStream>>()
        .await?;
    Ok(resp)
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DispatcharrEpgData {
    pub id: i64,
    pub tvg_id: Option<String>,
}

/// Fetch every `EPGData` row — the per-source guide-channel bindings a
/// `Channel.epg_data_id` points at. Unlike `channels`/`streams`, this
/// endpoint returns a plain JSON array, not a paginated `{count,results}`
/// envelope.
pub(crate) async fn fetch_epg_data(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> Result<Vec<DispatcharrEpgData>> {
    let resp = client
        .get(format!("{base_url}/api/epg/epgdata/"))
        .header("X-API-Key", token)
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<DispatcharrEpgData>>()
        .await?;
    Ok(resp)
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DispatcharrProgram {
    pub start_time: chrono::DateTime<chrono::Utc>,
    pub end_time: chrono::DateTime<chrono::Utc>,
    pub title: String,
    #[serde(default)]
    pub sub_title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    pub tvg_id: String,
}

#[derive(Debug, Deserialize)]
struct EpgGridResponse {
    data: Vec<DispatcharrProgram>,
}

/// Fetch the EPG guide window (previous hour + current + next ~24h,
/// Dispatcharr-side) for every channel. `ProgramData.tvg_id` matches
/// `EPGData.tvg_id` — resolve each channel's `epg_data_id` through
/// `fetch_epg_data()` to get the right join key, not the channel's own
/// (possibly stale) `tvg_id` field. Also not Dispatcharr's separate
/// `/output/epg` XMLTV export, whose `<channel id>` attribute is a
/// re-numbered export identifier that doesn't correspond to any `tvg_id`.
pub(crate) async fn fetch_epg_grid(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> Result<Vec<DispatcharrProgram>> {
    let resp = client
        .get(format!("{base_url}/api/epg/grid/"))
        .header("X-API-Key", token)
        .send()
        .await?
        .error_for_status()?
        .json::<EpgGridResponse>()
        .await?;
    Ok(resp.data)
}

/// Best-effort resolution quality tag pulled from Dispatcharr's free-form
/// `stream_stats` probe blob, so 4K/HD streams are labeled distinctly in the
/// version picker (per house rule: always surface 4K variants distinctly).
fn quality_tag(stats: &Option<serde_json::Value>) -> Option<String> {
    let stats = stats.as_ref()?;
    let height = stats
        .get("resolution")
        .and_then(|r| r.get("height"))
        .and_then(|h| h.as_i64())
        .or_else(|| {
            stats
                .get("height")
                .and_then(|h| h.as_i64())
        })?;
    Some(match height {
        h if h >= 2000 => "4K".to_string(),
        h if h >= 1000 => "1080p".to_string(),
        h if h >= 700 => "720p".to_string(),
        h => format!("{h}p"),
    })
}

pub(crate) fn channel_to_media(
    ch: &DispatcharrChannel,
    addon_id: Uuid,
    source_id: &str,
) -> db::Media {
    db::Media {
        id: Uuid::new_v5(&addon_id, format!("channel:{}", ch.id).as_bytes()),
        title: ch
            .name
            .clone(),
        kind: db::MediaKind::TvChannel,
        tvg_id: ch
            .tvg_id
            .clone()
            .filter(|s| !s.is_empty()),
        channel_number: ch
            .channel_number
            .map(|n| n as i64),
        external_ids: db::ExternalIds {
            iptv_source_id: Some(source_id.to_owned()),
            ..Default::default()
        },
        enabled: true,
        ..Default::default()
    }
}

pub(crate) fn stream_to_media(
    stream: &DispatcharrStream,
    channel_media_id: Uuid,
    idx: i64,
    playback_id: &str,
    base_url: &str,
    api_key: &str,
    now: chrono::NaiveDateTime,
) -> db::Media {
    let title = match quality_tag(&stream.stream_stats) {
        Some(tag) => format!("{} ({tag})", stream.name),
        None => stream
            .name
            .clone(),
    };
    // No trailing slash: `/proxy/ts/stream/<id>/` (with a trailing slash)
    // falls through to Dispatcharr's SPA catch-all and returns an HTML page,
    // not the stream. Requires its own X-API-Key
    // header, which a Jellyfin client can't supply — so this can't be
    // direct-played by the client; it's fetched through remux's own generic
    // `GET /stream/{id}` proxy (`HttpSource::serve_inner`, which forwards
    // `request_headers`), keyed by *this* Media row's own id, not linked to
    // directly.
    db::Media {
        id: Uuid::new_v5(
            &channel_media_id,
            format!("stream:{}", stream.id).as_bytes(),
        ),
        title,
        kind: db::MediaKind::Stream,
        parent_id: Some(channel_media_id),
        idx: Some(idx),
        // Explicit, not the constructor-time default: this must be >= the
        // parent's `streams_refreshed_at` marker set right after this upsert,
        // or `.streams()`'s freshness filter (`updated_at >= refreshed`)
        // would immediately hide every version just synced.
        created_at: now,
        updated_at: now,
        stream_info: Some(crate::stream::StreamInfo {
            descriptor: crate::stream::StreamDescriptor::Http {
                url: format!("{base_url}/proxy/ts/stream/{playback_id}"),
                request_headers: std::collections::HashMap::from([(
                    "X-API-Key".to_string(),
                    api_key.to_string(),
                )]),
                response_headers: Default::default(),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Builds the parent `Recording` row for one Dispatcharr DVR recording —
/// same shape `channel_to_media` uses for channels. The playable file itself
/// is a separate `Stream`-kind child (`recording_stream_to_media`), never
/// this row's own `stream_info`.
pub(crate) fn recording_to_media(
    rec: &super::dispatcharr_dvr::DispatcharrRecording,
    addon_id: Uuid,
    source_id: &str,
) -> db::Media {
    let id = Uuid::new_v5(&addon_id, format!("recording:{}", rec.id).as_bytes());
    db::Media {
        id,
        title: rec
            .program_title()
            .unwrap_or("Recording")
            .to_string(),
        kind: db::MediaKind::Recording,
        description: rec
            .program_description()
            .map(str::to_owned),
        live_start: Some(
            rec.start_time
                .naive_utc(),
        ),
        live_end: Some(
            rec.end_time
                .naive_utc(),
        ),
        runtime: Some(
            (rec.end_time - rec.start_time)
                .num_seconds()
                .max(0),
        ),
        parent_id: Some(Uuid::new_v5(
            &addon_id,
            format!("channel:{}", rec.channel).as_bytes(),
        )),
        external_ids: db::ExternalIds {
            iptv_source_id: Some(source_id.to_owned()),
            dispatcharr_recording_id: Some(rec.id),
            ..Default::default()
        },
        enabled: true,
        ..Default::default()
    }
}

/// The single playable `Stream`-kind child of a synced `Recording` row.
/// `stream_info` points at Dispatcharr's real `/file/` URL (`X-API-Key`
/// header attached, same as `stream_to_media` does for channels) — this is
/// the URL `StreamDescriptor::server_input` hands to the internal ffprobe
/// pass, which needs a genuinely fetchable absolute address, not the
/// client-facing `/livetv/liverecordings/{id}/stream` Path set separately
/// in `api::db_media_to_item`'s `Recording` block.
pub(crate) fn recording_stream_to_media(
    rec: &super::dispatcharr_dvr::DispatcharrRecording,
    recording_media_id: Uuid,
    base_url: &str,
    api_key: &str,
    now: chrono::NaiveDateTime,
) -> db::Media {
    db::Media {
        id: Uuid::new_v5(&recording_media_id, b"stream"),
        title: rec
            .program_title()
            .unwrap_or("Recording")
            .to_string(),
        kind: db::MediaKind::Stream,
        parent_id: Some(recording_media_id),
        idx: Some(0),
        created_at: now,
        updated_at: now,
        stream_info: Some(crate::stream::StreamInfo {
            descriptor: crate::stream::StreamDescriptor::Http {
                url: format!("{base_url}/api/channels/recordings/{}/file/", rec.id),
                request_headers: std::collections::HashMap::from([(
                    "X-API-Key".to_string(),
                    api_key.to_string(),
                )]),
                response_headers: Default::default(),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Runtime addon
// ---------------------------------------------------------------------------

pub(crate) struct DispatcharrAddon {
    pub(crate) addon_id: Uuid,
    pub(crate) base_url: String,
    pub(crate) api_key: String,
}

impl DispatcharrAddon {
    pub(crate) fn source_id(&self) -> String {
        self.addon_id
            .simple()
            .to_string()
    }
}

#[async_trait]
impl AddonKind for DispatcharrAddon {
    fn id(&self) -> &'static str {
        "dispatcharr"
    }

    async fn available_info(
        &self,
    ) -> Result<Option<(Vec<remux_sdks::stremio::ResourceRef>, Vec<StremioMediaType>)>>
    {
        let make_ref = |name| remux_sdks::stremio::ResourceRef {
            name,
            types: vec![],
            id_prefixes: None,
        };
        Ok(Some((
            vec![
                make_ref(ResourceType::Stream),
                make_ref(ResourceType::Catalog),
            ],
            vec![StremioMediaType::Tv],
        )))
    }
}

#[async_trait]
impl CatalogAddon for DispatcharrAddon {
    async fn catalog_list(&self, _ctx: &AppContext) -> Result<Vec<CatalogInfo>> {
        Ok(vec![CatalogInfo {
            provider_catalog_id: "channels".to_string(),
            name: "Dispatcharr Channels".to_string(),
            default_enabled: true,
            default_max_items: Some(999999999),
            collection_media_kind: None,
            media_kind: Some(db::MediaKind::TvChannel),
        }])
    }

    async fn catalog_stream(
        &self,
        _ctx: &AppContext,
        local_id: &str,
    ) -> Result<Option<Pin<Box<dyn Stream<Item = db::Media> + Send>>>> {
        if local_id != "channels" {
            return Ok(None);
        }

        let client = reqwest::Client::new();
        let source_id = self.source_id();
        let addon_id = self.addon_id;

        debug!(base_url = %self.base_url, "fetching Dispatcharr channels");
        let channels = fetch_channels(&client, &self.base_url, &self.api_key).await?;
        let items: Vec<db::Media> = channels
            .iter()
            .map(|ch| channel_to_media(ch, addon_id, &source_id))
            .collect();

        Ok(Some(Box::pin(futures::stream::iter(items))))
    }
}

#[async_trait]
impl StreamAddon for DispatcharrAddon {
    fn supports(&self, media: &db::Media) -> bool {
        media.kind == db::MediaKind::TvChannel
    }

    async fn get_streams(
        &self,
        media: &db::Media,
        _ctx: &AppContext,
        _id_prefixes: Option<&[String]>,
    ) -> Result<Vec<crate::stream::StreamInfo>> {
        // Channel playback is never dynamically dispatched per-request (see
        // `services::stream_service::dispatch_lookup`, which short-circuits for
        // TvChannel) — versions come solely from the synced `Stream`-kind
        // children written by `RefreshDispatcharrStreamsTask`. This impl only
        // exists to satisfy `AddonCapabilities`; echo whatever is already on
        // the row, same as `IptvAddon`.
        let Some(ref si) = media.stream_info else {
            return Ok(vec![]);
        };
        Ok(vec![si.clone()])
    }
}
