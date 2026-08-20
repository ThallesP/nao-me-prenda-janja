//! Thin client for the Cloudflare Realtime SFU HTTPS API.
//! The browser never talks to Cloudflare's API directly — everything is
//! proxied through us so the app secret stays server-side and we can enforce
//! who may publish or pull which tracks.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::state::AppState;

const BASE: &str = "https://rtc.live.cloudflare.com/v1/apps";

#[derive(Serialize)]
pub struct LocalTrack<'a> {
    pub location: &'static str, // "local"
    pub mid: &'a str,
    #[serde(rename = "trackName")]
    pub track_name: String,
}

#[derive(Serialize)]
pub struct RemoteTrack {
    pub location: &'static str, // "remote"
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "trackName")]
    pub track_name: String,
}

#[derive(Deserialize)]
pub struct SessionDescription {
    pub sdp: String,
}

#[derive(Deserialize)]
pub struct TrackResult {
    pub mid: Option<String>,
    #[serde(rename = "trackName")]
    pub track_name: Option<String>,
    #[serde(rename = "errorCode")]
    pub error_code: Option<String>,
    #[serde(rename = "errorDescription")]
    pub error_description: Option<String>,
}

#[derive(Deserialize)]
pub struct TracksResponse {
    #[serde(rename = "sessionDescription")]
    pub session_description: Option<SessionDescription>,
    #[serde(default)]
    pub tracks: Vec<TrackResult>,
    #[serde(rename = "requiresImmediateRenegotiation", default)]
    pub requires_immediate_renegotiation: bool,
    #[serde(rename = "errorCode")]
    pub error_code: Option<String>,
    #[serde(rename = "errorDescription")]
    pub error_description: Option<String>,
}

fn base_url(state: &AppState) -> String {
    format!("{BASE}/{}", state.cfg.cloudflare().app_id)
}

pub async fn new_session(state: &AppState) -> Result<String> {
    #[derive(Deserialize)]
    struct NewSession {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "errorCode")]
        error_code: Option<String>,
    }
    let resp = state
        .http
        .post(format!("{}/sessions/new", base_url(state)))
        .bearer_auth(&state.cfg.cloudflare().app_secret)
        .send()
        .await
        .context("cloudflare sessions/new request")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("cloudflare sessions/new failed ({status}): {body}");
    }
    let parsed: NewSession = resp.json().await.context("sessions/new parse")?;
    if let Some(code) = parsed.error_code {
        bail!("cloudflare sessions/new error: {code}");
    }
    Ok(parsed.session_id)
}

/// Publish local tracks: send the browser's SDP offer, get the SFU's answer.
pub async fn publish_tracks(
    state: &AppState,
    session_id: &str,
    offer_sdp: &str,
    tracks: &[LocalTrack<'_>],
) -> Result<TracksResponse> {
    tracks_new(
        state,
        session_id,
        &json!({
            "sessionDescription": { "type": "offer", "sdp": offer_sdp },
            "tracks": tracks,
        }),
    )
    .await
}

/// Pull remote tracks into a (viewer) session. The SFU responds with an offer
/// that the viewer must answer via `renegotiate`.
pub async fn pull_tracks(
    state: &AppState,
    session_id: &str,
    tracks: &[RemoteTrack],
) -> Result<TracksResponse> {
    tracks_new(state, session_id, &json!({ "tracks": tracks })).await
}

async fn tracks_new(
    state: &AppState,
    session_id: &str,
    body: &serde_json::Value,
) -> Result<TracksResponse> {
    let resp = state
        .http
        .post(format!("{}/sessions/{session_id}/tracks/new", base_url(state)))
        .bearer_auth(&state.cfg.cloudflare().app_secret)
        .json(body)
        .send()
        .await
        .context("cloudflare tracks/new request")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("cloudflare tracks/new failed ({status}): {body}");
    }
    let parsed: TracksResponse = resp.json().await.context("tracks/new parse")?;
    if let Some(code) = &parsed.error_code {
        bail!(
            "cloudflare tracks/new error: {code} {}",
            parsed.error_description.as_deref().unwrap_or("")
        );
    }
    Ok(parsed)
}

/// Close pulled tracks so the SFU stops sending them. The browser stops the
/// transceivers and offers; Cloudflare answers with those m-lines rejected.
pub async fn close_tracks(
    state: &AppState,
    session_id: &str,
    mids: &[String],
    offer_sdp: &str,
) -> Result<String> {
    let tracks: Vec<serde_json::Value> = mids.iter().map(|mid| json!({ "mid": mid })).collect();
    let resp = state
        .http
        .put(format!("{}/sessions/{session_id}/tracks/close", base_url(state)))
        .bearer_auth(&state.cfg.cloudflare().app_secret)
        .json(&json!({
            "tracks": tracks,
            "sessionDescription": { "type": "offer", "sdp": offer_sdp },
            "force": false,
        }))
        .send()
        .await
        .context("cloudflare tracks/close request")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("cloudflare tracks/close failed ({status}): {body}");
    }
    let parsed: TracksResponse = resp.json().await.context("tracks/close parse")?;
    if let Some(code) = &parsed.error_code {
        bail!(
            "cloudflare tracks/close error: {code} {}",
            parsed.error_description.as_deref().unwrap_or("")
        );
    }
    // A per-track failure means the SFU may still be forwarding that mid;
    // reporting success would desync the browser's mid bookkeeping.
    for track in &parsed.tracks {
        if let Some(code) = &track.error_code {
            bail!(
                "cloudflare tracks/close track error: {code} {}",
                track.error_description.as_deref().unwrap_or("")
            );
        }
    }
    parsed
        .session_description
        .map(|d| d.sdp)
        .ok_or_else(|| anyhow::anyhow!("cloudflare tracks/close returned no answer"))
}

/// Immediately stop media on every supplied mid without requiring a browser
/// offer/answer round. This is the server-side revocation path used when a
/// websocket lease, share, or signed session expires.
pub async fn force_close_tracks(
    state: &AppState,
    session_id: &str,
    mids: &[String],
) -> Result<()> {
    for chunk in mids.chunks(64) {
        force_close_track_chunk(state, session_id, chunk).await?;
    }
    Ok(())
}

async fn force_close_track_chunk(
    state: &AppState,
    session_id: &str,
    mids: &[String],
) -> Result<()> {
    let tracks: Vec<serde_json::Value> = mids.iter().map(|mid| json!({ "mid": mid })).collect();
    let resp = state
        .http
        .put(format!("{}/sessions/{session_id}/tracks/close", base_url(state)))
        .bearer_auth(&state.cfg.cloudflare().app_secret)
        .json(&json!({ "tracks": tracks, "force": true }))
        .send()
        .await
        .context("cloudflare forced tracks/close request")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("cloudflare forced tracks/close failed ({status}): {body}");
    }
    let parsed: TracksResponse = resp.json().await.context("forced tracks/close parse")?;
    if let Some(code) = &parsed.error_code {
        bail!(
            "cloudflare forced tracks/close error: {code} {}",
            parsed.error_description.as_deref().unwrap_or("")
        );
    }
    for track in &parsed.tracks {
        if let Some(code) = &track.error_code {
            bail!(
                "cloudflare forced tracks/close track error: {code} {}",
                track.error_description.as_deref().unwrap_or("")
            );
        }
    }
    Ok(())
}

pub async fn renegotiate(state: &AppState, session_id: &str, answer_sdp: &str) -> Result<()> {
    let resp = state
        .http
        .put(format!("{}/sessions/{session_id}/renegotiate", base_url(state)))
        .bearer_auth(&state.cfg.cloudflare().app_secret)
        .json(&json!({
            "sessionDescription": { "type": "answer", "sdp": answer_sdp }
        }))
        .send()
        .await
        .context("cloudflare renegotiate request")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("cloudflare renegotiate failed ({status}): {body}");
    }
    Ok(())
}
