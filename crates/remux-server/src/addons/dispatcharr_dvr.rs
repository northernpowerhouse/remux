//! Raw Dispatcharr DVR HTTP client: recordings + series rules.
//!
//! Sibling of `addons/dispatcharr.rs`'s catalog/EPG `fetch_*` functions, same
//! conventions (plain `reqwest::Client` + `X-API-Key` header). These
//! endpoints return plain arrays (or `{"rules": […]}` for series rules), not
//! DRF's `{count,results}` pagination envelope.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DispatcharrRecording {
    pub id: i64,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub channel: i64,
    #[serde(default)]
    pub custom_properties: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatcharrRecordingStatus {
    /// `custom_properties` carries no `status` key at all for an upcoming,
    /// not-yet-started recording.
    Scheduled,
    Recording,
    Completed,
    Failed,
    Stopped,
    Interrupted,
}

impl DispatcharrRecording {
    pub(crate) fn status(&self) -> DispatcharrRecordingStatus {
        match self
            .custom_properties
            .get("status")
            .and_then(Value::as_str)
        {
            Some("recording") => DispatcharrRecordingStatus::Recording,
            Some("completed") => DispatcharrRecordingStatus::Completed,
            Some("failed") => DispatcharrRecordingStatus::Failed,
            Some("stopped") => DispatcharrRecordingStatus::Stopped,
            Some("interrupted") => DispatcharrRecordingStatus::Interrupted,
            _ => DispatcharrRecordingStatus::Scheduled,
        }
    }

    pub(crate) fn program_title(&self) -> Option<&str> {
        self.custom_properties
            .get("program")
            .and_then(|p| p.get("title"))
            .and_then(Value::as_str)
            .or_else(|| {
                self.custom_properties
                    .get("title")
                    .and_then(Value::as_str)
            })
    }

    pub(crate) fn program_description(&self) -> Option<&str> {
        self.custom_properties
            .get("program")
            .and_then(|p| p.get("description"))
            .and_then(Value::as_str)
            .or_else(|| {
                self.custom_properties
                    .get("description")
                    .and_then(Value::as_str)
            })
    }

    /// Authoritative playback pointer, regardless of state: a `/file/` path
    /// once finished, an `/hls/index.m3u8` path while still recording — see
    /// Dispatcharr's own `RecordingViewSet.file` docstring, which redirects
    /// there itself. Prefer this over reconstructing a URL from `id`.
    pub(crate) fn file_url(&self) -> Option<&str> {
        self.custom_properties
            .get("file_url")
            .or_else(|| {
                self.custom_properties
                    .get("output_file_url")
            })
            .and_then(Value::as_str)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct DispatcharrSeriesRule {
    #[serde(default)]
    pub tvg_id: Option<String>,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub epg_source_id: Option<i64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SeriesRuleRequest {
    pub tvg_id: Option<String>,
    pub mode: String,
    pub title: Option<String>,
    pub epg_source_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct SeriesRulesResponse {
    rules: Vec<DispatcharrSeriesRule>,
}

pub(crate) async fn schedule_recording(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    channel_id: i64,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<DispatcharrRecording> {
    let resp = client
        .post(format!("{base_url}/api/channels/recordings/"))
        .header("X-API-Key", token)
        .json(&serde_json::json!({
            "channel": channel_id,
            "start_time": start.to_rfc3339(),
            "end_time": end.to_rfc3339(),
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<DispatcharrRecording>()
        .await?;
    Ok(resp)
}

pub(crate) async fn list_recordings(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> Result<Vec<DispatcharrRecording>> {
    let resp = client
        .get(format!("{base_url}/api/channels/recordings/"))
        .header("X-API-Key", token)
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<DispatcharrRecording>>()
        .await?;
    Ok(resp)
}

pub(crate) async fn get_recording(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    id: i64,
) -> Result<DispatcharrRecording> {
    let resp = client
        .get(format!("{base_url}/api/channels/recordings/{id}/"))
        .header("X-API-Key", token)
        .send()
        .await?
        .error_for_status()?
        .json::<DispatcharrRecording>()
        .await?;
    Ok(resp)
}

pub(crate) async fn delete_recording(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    id: i64,
) -> Result<()> {
    client
        .delete(format!("{base_url}/api/channels/recordings/{id}/"))
        .header("X-API-Key", token)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

pub(crate) async fn stop_recording(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    id: i64,
) -> Result<DispatcharrRecording> {
    let resp = client
        .post(format!("{base_url}/api/channels/recordings/{id}/stop/"))
        .header("X-API-Key", token)
        .send()
        .await?
        .error_for_status()?
        .json::<DispatcharrRecording>()
        .await?;
    Ok(resp)
}

pub(crate) async fn list_series_rules(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> Result<Vec<DispatcharrSeriesRule>> {
    let resp = client
        .get(format!("{base_url}/api/channels/series-rules/"))
        .header("X-API-Key", token)
        .send()
        .await?
        .error_for_status()?
        .json::<SeriesRulesResponse>()
        .await?;
    Ok(resp.rules)
}

pub(crate) async fn create_series_rule(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    rule: &SeriesRuleRequest,
) -> Result<Vec<DispatcharrSeriesRule>> {
    let resp = client
        .post(format!("{base_url}/api/channels/series-rules/"))
        .header("X-API-Key", token)
        .json(rule)
        .send()
        .await?
        .error_for_status()?
        .json::<SeriesRulesResponse>()
        .await?;
    Ok(resp.rules)
}

pub(crate) async fn delete_series_rule(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    tvg_id: Option<&str>,
    title: Option<&str>,
    epg_source_id: Option<i64>,
) -> Result<()> {
    // reqwest 0.13's `.query()` builder needs the (unenabled) `query`
    // feature — build the query string by hand instead.
    let mut params = Vec::new();
    if let Some(v) = tvg_id {
        params.push(format!("tvg_id={}", urlencoding::encode(v)));
    }
    if let Some(v) = title {
        params.push(format!("title={}", urlencoding::encode(v)));
    }
    if let Some(v) = epg_source_id {
        params.push(format!("epg_source_id={v}"));
    }
    let qs = if params.is_empty() {
        String::new()
    } else {
        format!("?{}", params.join("&"))
    };
    client
        .delete(format!("{base_url}/api/channels/series-rules/{qs}"))
        .header("X-API-Key", token)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}
