use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::{self, Claims, Role, SESSION_TTL_SECS};
use crate::config::RtcBackend;
use crate::state::{
    AppState, PendingCode, RtcLimitError, RtcSessionOwner, ServerMsg, Share,
};
use crate::{discord, realtime, ws};

const SHARE_CODE_TTL_SECS: u64 = 300;
const MAX_SHARE_CODES_PER_USER: usize = 8;
const MAX_SHARE_CODES_TOTAL: usize = 1024;
const MAX_RTC_TRACKS_PER_SESSION: usize = 64;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/config-public", get(config_public))
        .route("/api/auth/token", post(auth_token))
        .route("/api/share-link", post(share_link))
        .route("/api/share/claim", post(share_claim))
        .route("/api/share/stop", post(share_stop))
        .route("/api/rtc/session", post(rtc_session_new))
        .route("/api/rtc/session/{id}", delete(rtc_session_close))
        .route("/api/rtc/session/{id}/publish", post(rtc_publish))
        .route("/api/rtc/session/{id}/pull", post(rtc_pull))
        .route("/api/rtc/session/{id}/unpull", post(rtc_unpull))
        .route("/api/rtc/session/{id}/renegotiate", put(rtc_renegotiate))
        .route("/api/ws", get(ws::upgrade))
        .route(
            "/api/debug",
            post(debug_report).layer(DefaultBodyLimit::max(4096)),
        )
        .fallback(crate::assets::serve)
        .layer(middleware::from_fn(log_requests))
        .with_state(state)
}

#[derive(Deserialize)]
struct DebugReport {
    step: String,
    referrer: String,
    page: String,
    params: Vec<String>,
    ua: String,
}

/// Clients post here when the Discord handshake stalls. The endpoint remains
/// available before login, but accepts only bounded structured fields and a
/// small process-wide rate so it cannot become an unauthenticated log sink.
async fn debug_report(
    State(state): State<Arc<AppState>>,
    Json(body): Json<DebugReport>,
) -> Result<StatusCode, ApiError> {
    if body.params.len() > 20 {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "too many query parameters"));
    }
    if !state.allow_debug_report() {
        return Err(ApiError::too_many_requests("debug report rate limit reached"));
    }
    let params: Vec<String> = body.params.iter().map(|value| clean_log_field(value, 80)).collect();
    tracing::warn!(
        step = ?clean_log_field(&body.step, 40),
        referrer = ?clean_log_field(&body.referrer, 512),
        page = ?clean_log_field(&body.page, 256),
        params = ?params,
        user_agent = ?clean_log_field(&body.ua, 512),
        "client Discord handshake stalled"
    );
    Ok(StatusCode::NO_CONTENT)
}

fn clean_log_field(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .take(max_chars)
        .map(|ch| if ch.is_control() { '\u{fffd}' } else { ch })
        .collect()
}

async fn log_requests(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = next.run(req).await;
    // /api/debug has its own accepted-report rate limit. Logging every rejected
    // request here would recreate the unauthenticated log-volume sink.
    if path.starts_with("/api") && path != "/api/debug" {
        tracing::info!("{method} {path} -> {}", resp.status());
    }
    resp
}

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
    pub fn unauthorized(msg: &str) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, msg)
    }
    pub fn forbidden(msg: &str) -> Self {
        Self::new(StatusCode::FORBIDDEN, msg)
    }
    pub fn not_found(msg: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, msg)
    }
    pub fn too_many_requests(msg: &str) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, msg)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        tracing::error!("internal error: {err:#}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

pub fn require_auth(state: &AppState, headers: &HeaderMap) -> Result<Claims, ApiError> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::unauthorized("missing bearer token"))?;
    auth::verify(&state.cfg.auth.session_secret, token)
        .ok_or_else(|| ApiError::unauthorized("invalid or expired token"))
}

/// Media/link grants require a live websocket connection in the room. The
/// websocket is where membership gets re-verified against Discord, so tying
/// HTTP grants to it means a token alone (e.g. after leaving the channel and
/// getting kicked) can't keep pulling streams.
async fn require_live_peer(state: &AppState, claims: &Claims) -> Result<(), ApiError> {
    let rooms = state.rooms.read().await;
    let alive = rooms
        .get(&claims.inst)
        .is_some_and(|room| {
            room.peers
                .values()
                .any(|p| p.user_id == claims.sub && p.role == claims.role)
        });
    if !alive {
        return Err(ApiError::forbidden("no active connection to this activity"));
    }
    Ok(())
}

async fn require_live_connection(
    state: &AppState,
    claims: &Claims,
    conn_id: u64,
) -> Result<(), ApiError> {
    let rooms = state.rooms.read().await;
    let alive = rooms
        .get(&claims.inst)
        .and_then(|room| room.peers.get(&conn_id))
        .is_some_and(|peer| peer.user_id == claims.sub && peer.role == claims.role);
    if !alive {
        return Err(ApiError::forbidden("rtc websocket lease is no longer active"));
    }
    Ok(())
}

async fn config_public(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({ "client_id": state.cfg.discord.client_id }))
}

#[derive(Deserialize)]
struct AuthTokenBody {
    code: String,
    instance_id: String,
}

#[derive(Serialize)]
struct AuthTokenResponse {
    session_token: String,
    access_token: String,
    user: PublicUser,
}

#[derive(Serialize)]
struct PublicUser {
    id: String,
    name: String,
    avatar: Option<String>,
}

async fn auth_token(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AuthTokenBody>,
) -> ApiResult<AuthTokenResponse> {
    let token = discord::exchange_code(&state, &body.code).await?;
    let user = discord::fetch_user(&state, &token.access_token).await?;
    let check = discord::check_instance_participant(&state, &body.instance_id, &user.id).await?;
    if !check.is_participant {
        return Err(ApiError::forbidden("not a participant of this activity instance"));
    }
    let (chan, guild) = check
        .location
        .map(|l| (l.channel_id, l.guild_id))
        .unwrap_or((None, None));
    let claims = Claims {
        sub: user.id.clone(),
        name: user.display_name(),
        avatar: user.avatar.clone(),
        inst: body.instance_id,
        chan,
        guild,
        role: Role::Member,
        exp: auth::now_secs() + SESSION_TTL_SECS,
    };
    let session_token = auth::mint(&state.cfg.auth.session_secret, &claims);
    Ok(Json(AuthTokenResponse {
        session_token,
        access_token: token.access_token,
        user: PublicUser { id: claims.sub, name: claims.name, avatar: claims.avatar },
    }))
}

#[derive(Serialize)]
struct ShareLinkResponse {
    url: String,
}

/// Mint a one-time code the user opens on the external page (outside the
/// activity iframe, where getDisplayMedia is actually allowed).
async fn share_link(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<ShareLinkResponse> {
    let claims = require_auth(&state, &headers)?;
    require_live_peer(&state, &claims).await?;
    let code = auth::random_id(16);
    let now = auth::now_secs();
    // Inherits the parent token's expiry — a share link must not extend a
    // session's lifetime.
    let publisher_claims = Claims { role: Role::Publisher, ..claims };
    let mut codes = state.share_codes.write().await;
    codes.retain(|_, pending| pending.expires_at > now);
    if codes.len() >= MAX_SHARE_CODES_TOTAL {
        return Err(ApiError::too_many_requests("share link capacity reached"));
    }
    let owned = codes.values().filter(|pending| pending.claims.sub == publisher_claims.sub).count();
    if owned >= MAX_SHARE_CODES_PER_USER {
        return Err(ApiError::too_many_requests("too many pending share links"));
    }
    codes.insert(
        code.clone(),
        PendingCode { claims: publisher_claims, expires_at: now + SHARE_CODE_TTL_SECS },
    );
    let url = format!("https://{}/share?code={code}", state.cfg.server.public_domain);
    Ok(Json(ShareLinkResponse { url }))
}

#[derive(Deserialize)]
struct ShareClaimBody {
    code: String,
}

#[derive(Serialize)]
struct ShareClaimResponse {
    session_token: String,
    user: PublicUser,
}

async fn share_claim(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ShareClaimBody>,
) -> ApiResult<ShareClaimResponse> {
    let pending = state
        .share_codes
        .write()
        .await
        .remove(&body.code)
        .ok_or_else(|| ApiError::unauthorized("invalid or already-used share code"))?;
    if pending.expires_at < auth::now_secs() {
        return Err(ApiError::unauthorized("share code expired"));
    }
    let claims = pending.claims;
    let session_token = auth::mint(&state.cfg.auth.session_secret, &claims);
    Ok(Json(ShareClaimResponse {
        session_token,
        user: PublicUser { id: claims.sub, name: claims.name, avatar: claims.avatar },
    }))
}

#[derive(Deserialize)]
struct ShareStopBody {
    share_id: String,
}

async fn share_stop(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ShareStopBody>,
) -> ApiResult<serde_json::Value> {
    let claims = require_auth(&state, &headers)?;
    let mut rooms = state.rooms.write().await;
    let room = rooms.get_mut(&claims.inst).ok_or_else(|| ApiError::not_found("room not found"))?;
    let owns = room.shares.get(&body.share_id).is_some_and(|s| s.user_id == claims.sub);
    if !owns {
        return Err(ApiError::forbidden("not your share"));
    }
    state.reap_share(room.end_share(&body.share_id));
    Ok(Json(json!({ "ok": true })))
}

#[derive(Serialize)]
struct SessionNewResponse {
    session_id: String,
}

#[derive(Deserialize)]
struct SessionNewBody {
    conn_id: u64,
}

async fn rtc_session_new(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<SessionNewBody>,
) -> ApiResult<SessionNewResponse> {
    let claims = require_auth(&state, &headers)?;
    require_live_connection(&state, &claims, body.conn_id).await?;
    let owner = RtcSessionOwner {
        user_id: claims.sub.clone(),
        instance_id: claims.inst.clone(),
        role: claims.role,
        conn_id: body.conn_id,
        expires_at: claims.exp,
        tracks: HashSet::new(),
    };
    let reservation_id = state.reserve_rtc_session(owner).await.map_err(|limit| match limit {
        RtcLimitError::PerUser => ApiError::too_many_requests("per-user rtc session limit reached"),
        RtcLimitError::Total => ApiError::too_many_requests("rtc session capacity reached"),
        RtcLimitError::PerUserRate => {
            ApiError::too_many_requests("rtc session creation rate reached")
        }
        RtcLimitError::TotalRate => {
            ApiError::too_many_requests("rtc session creation capacity reached")
        }
    })?;
    let allocated = match state.cfg.rtc.backend {
        RtcBackend::Cloudflare => realtime::new_session(&state).await.map_err(ApiError::from),
        RtcBackend::Builtin => {
            state.builtin_sfu().new_session(&claims.sub).await.map_err(ApiError::from)
        }
    };
    let session_id = match allocated {
        Ok(session_id) => session_id,
        Err(err) => {
            state.cancel_rtc_reservation(&reservation_id).await;
            return Err(err);
        }
    };

    // Allocation is intentionally outside the registry lock. Re-check the
    // exact websocket lease, then atomically consume the reservation. Cleanup
    // cancels the reservation if the socket disappears during allocation.
    if require_live_connection(&state, &claims, body.conn_id).await.is_err()
        || !state.commit_rtc_session(&reservation_id, session_id.clone()).await
    {
        state.cancel_rtc_reservation(&reservation_id).await;
        state.close_backend_session(&session_id, &HashSet::new()).await;
        return Err(ApiError::forbidden("rtc websocket lease ended during allocation"));
    }
    Ok(Json(SessionNewResponse { session_id }))
}

async fn check_session_owner(
    state: &AppState,
    claims: &Claims,
    session_id: &str,
) -> Result<RtcSessionOwner, ApiError> {
    let owner = state
        .rtc_session_owner(session_id)
        .await
        .ok_or_else(|| ApiError::not_found("unknown rtc session"))?;
    if owner.user_id != claims.sub
        || owner.instance_id != claims.inst
        || owner.role != claims.role
    {
        return Err(ApiError::forbidden("not your rtc session"));
    }
    if owner.expires_at <= auth::now_secs() {
        return Err(ApiError::unauthorized("rtc session expired"));
    }
    Ok(owner)
}

/// Explicitly release a client peer connection. This keeps normal viewer
/// rebuilds and abandoned pre-publish attempts from consuming session quota.
async fn rtc_session_close(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<serde_json::Value> {
    let claims = require_auth(&state, &headers)?;
    let owner = check_session_owner(&state, &claims, &session_id).await?;
    {
        let mut rooms = state.rooms.write().await;
        if let Some(room) = rooms.get_mut(&claims.inst) {
            let shares: Vec<String> = room
                .shares
                .values()
                .filter(|share| {
                    share.publisher_session_id == session_id && share.conn_id == owner.conn_id
                })
                .map(|share| share.id.clone())
                .collect();
            for share_id in shares {
                room.end_share(&share_id);
            }
        }
    }
    state.revoke_rtc_session(&session_id).await;
    Ok(Json(json!({ "ok": true })))
}

async fn require_live_session(
    state: &AppState,
    claims: &Claims,
    session_id: &str,
) -> Result<RtcSessionOwner, ApiError> {
    let owner = check_session_owner(state, claims, session_id).await?;
    require_live_connection(state, claims, owner.conn_id).await?;
    Ok(owner)
}

async fn revoke_with_tracks(state: &Arc<AppState>, session_id: &str, mids: &[String]) {
    if state.record_rtc_tracks(session_id, mids).await {
        state.revoke_rtc_session(session_id).await;
    } else {
        let tracks: HashSet<String> = mids.iter().cloned().collect();
        state.close_backend_session(session_id, &tracks).await;
    }
}

#[derive(Deserialize)]
struct PublishBody {
    sdp: String,
    video_mid: String,
    audio_mid: Option<String>,
    conn_id: u64,
}

#[derive(Serialize)]
struct PublishResponse {
    share_id: String,
    answer_sdp: String,
}

async fn rtc_publish(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<PublishBody>,
) -> ApiResult<PublishResponse> {
    let claims = require_auth(&state, &headers)?;
    if claims.role != Role::Publisher {
        return Err(ApiError::forbidden("publishing requires the share page session"));
    }
    let owner = require_live_session(&state, &claims, &session_id).await?;
    if body.conn_id != owner.conn_id {
        return Err(ApiError::forbidden("rtc session belongs to a different websocket lease"));
    }

    let share_id = auth::random_id(8);
    // Track names are chosen server-side; clients never get to pick them.
    let mut tracks = vec![(body.video_mid.clone(), format!("{share_id}:v"))];
    if let Some(audio_mid) = &body.audio_mid {
        tracks.push((audio_mid.clone(), format!("{share_id}:a")));
    }
    let active_mids: Vec<String> = tracks.iter().map(|(mid, _)| mid.clone()).collect();
    let answer_sdp = match state.cfg.rtc.backend {
        RtcBackend::Cloudflare => {
            let cf_tracks: Vec<realtime::LocalTrack> = tracks
                .iter()
                .map(|(mid, track_name)| realtime::LocalTrack {
                    location: "local",
                    mid,
                    track_name: track_name.clone(),
                })
                .collect();
            let resp = match realtime::publish_tracks(&state, &session_id, &body.sdp, &cf_tracks)
                .await
            {
                Ok(resp) => resp,
                Err(err) => {
                    revoke_with_tracks(&state, &session_id, &active_mids).await;
                    return Err(err.into());
                }
            };
            for track in &resp.tracks {
                if let Some(code) = &track.error_code {
                    tracing::error!(
                        "cloudflare track error: {code} {}",
                        track.error_description.as_deref().unwrap_or("")
                    );
                    revoke_with_tracks(&state, &session_id, &active_mids).await;
                    return Err(ApiError::new(StatusCode::BAD_GATEWAY, "sfu rejected a track"));
                }
            }
            match resp.session_description {
                Some(description) => description.sdp,
                None => {
                    revoke_with_tracks(&state, &session_id, &active_mids).await;
                    return Err(ApiError::new(StatusCode::BAD_GATEWAY, "sfu returned no answer"));
                }
            }
        }
        RtcBackend::Builtin => {
            match state.builtin_sfu().publish(&session_id, &body.sdp, &tracks).await {
                Ok(answer) => answer,
                Err(err) => {
                    revoke_with_tracks(&state, &session_id, &active_mids).await;
                    return Err(err.into());
                }
            }
        }
    };

    if !state.record_rtc_tracks(&session_id, &active_mids).await {
        let mids = active_mids.iter().cloned().collect();
        state.close_backend_session(&session_id, &mids).await;
        return Err(ApiError::forbidden("rtc websocket lease ended during publish"));
    }

    let share = Share {
        id: share_id.clone(),
        user_id: claims.sub.clone(),
        username: claims.name.clone(),
        avatar: claims.avatar.clone(),
        has_audio: body.audio_mid.is_some(),
        started_at: auth::now_secs(),
        publisher_session_id: session_id.clone(),
        conn_id: body.conn_id,
    };

    let mut rooms = state.rooms.write().await;
    // Re-check under the write lock: if the publisher websocket dropped while
    // we were talking to Cloudflare, inserting now would leak an ownerless
    // share (its cleanup already ran).
    let conn_alive = rooms
        .get(&claims.inst)
        .and_then(|room| room.peers.get(&body.conn_id))
        .is_some_and(|peer| peer.user_id == claims.sub && peer.role == Role::Publisher);
    if !conn_alive {
        drop(rooms);
        state.revoke_rtc_session(&session_id).await;
        return Err(ApiError::forbidden("share page websocket disconnected"));
    }
    let room = rooms.get_mut(&claims.inst).expect("room checked above");
    // One live share per user, Discord-style: starting a new one replaces the old.
    let previous: Vec<String> = room
        .shares
        .values()
        .filter(|s| s.user_id == claims.sub)
        .map(|s| s.id.clone())
        .collect();
    for id in previous {
        let ended = room.end_share(&id);
        // Same RTC session republishing must not tear down its own peer
        // connection; anything else (a fresh share page) frees the old one.
        if ended.as_ref().is_some_and(|s| s.publisher_session_id != share.publisher_session_id) {
            state.reap_share(ended);
        }
    }
    room.broadcast(&ServerMsg::ShareStarted { share: &share });
    room.shares.insert(share_id.clone(), share);

    Ok(Json(PublishResponse { share_id, answer_sdp }))
}

#[derive(Deserialize)]
struct PullBody {
    share_id: String,
    /// Subset of ["video", "audio"] to pull; both when omitted. Lets viewers
    /// subscribe to just the audio (hidden tab) or just the video (muted).
    kinds: Option<Vec<String>>,
}

#[derive(Serialize)]
struct PullResponse {
    offer_sdp: String,
    tracks: Vec<PullTrack>,
    requires_renegotiation: bool,
}

#[derive(Serialize)]
struct PullTrack {
    mid: String,
    track_name: String,
}

async fn rtc_pull(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<PullBody>,
) -> ApiResult<PullResponse> {
    let claims = require_auth(&state, &headers)?;
    require_live_session(&state, &claims, &session_id).await?;

    if body.kinds.as_ref().is_some_and(|kinds| {
        kinds.len() > 2
            || kinds.iter().any(|kind| kind != "video" && kind != "audio")
            || kinds.iter().collect::<HashSet<_>>().len() != kinds.len()
    }) {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid track kinds"));
    }

    let wants = |kind: &str| body.kinds.as_ref().is_none_or(|k| k.iter().any(|s| s == kind));
    let (publisher_session_id, track_names) = {
        let rooms = state.rooms.read().await;
        let share = rooms
            .get(&claims.inst)
            .and_then(|room| room.shares.get(&body.share_id))
            .ok_or_else(|| ApiError::not_found("share not found"))?;
        let mut names = Vec::new();
        if wants("video") {
            names.push(format!("{}:v", share.id));
        }
        if share.has_audio && wants("audio") {
            names.push(format!("{}:a", share.id));
        }
        if names.is_empty() {
            return Err(ApiError::new(StatusCode::BAD_REQUEST, "nothing to pull"));
        }
        (share.publisher_session_id.clone(), names)
    };

    let (offer_sdp, tracks, requires_renegotiation) = match state.cfg.rtc.backend {
        RtcBackend::Cloudflare => {
            let remote_tracks: Vec<realtime::RemoteTrack> = track_names
                .iter()
                .map(|name| realtime::RemoteTrack {
                    location: "remote",
                    session_id: publisher_session_id.clone(),
                    track_name: name.clone(),
                })
                .collect();
            let resp = match realtime::pull_tracks(&state, &session_id, &remote_tracks).await {
                Ok(resp) => resp,
                Err(err) => {
                    state.revoke_rtc_session(&session_id).await;
                    return Err(err.into());
                }
            };
            for track in &resp.tracks {
                if let Some(code) = &track.error_code {
                    tracing::error!(
                        "cloudflare pull track error: {code} {}",
                        track.error_description.as_deref().unwrap_or("")
                    );
                    let mids: Vec<String> =
                        resp.tracks.iter().filter_map(|track| track.mid.clone()).collect();
                    revoke_with_tracks(&state, &session_id, &mids).await;
                    return Err(ApiError::new(StatusCode::BAD_GATEWAY, "sfu rejected a pull"));
                }
            }
            let offer_sdp = match resp.session_description {
                Some(description) => description.sdp,
                None => {
                    let mids: Vec<String> =
                        resp.tracks.iter().filter_map(|track| track.mid.clone()).collect();
                    revoke_with_tracks(&state, &session_id, &mids).await;
                    return Err(ApiError::new(StatusCode::BAD_GATEWAY, "sfu returned no offer"));
                }
            };
            let requires_renegotiation = resp.requires_immediate_renegotiation;
            let tracks: Vec<PullTrack> = resp
                .tracks
                .into_iter()
                .filter_map(|t| match (t.mid, t.track_name) {
                    (Some(mid), Some(track_name)) => Some(PullTrack { mid, track_name }),
                    _ => None,
                })
                .collect();
            (offer_sdp, tracks, requires_renegotiation)
        }
        RtcBackend::Builtin => {
            let (offer_sdp, pairs) =
                match state.builtin_sfu().subscribe(&session_id, &track_names).await {
                    Ok(response) => response,
                    Err(err) => {
                        state.revoke_rtc_session(&session_id).await;
                        return Err(err.into());
                    }
                };
            let tracks: Vec<PullTrack> = pairs
                .into_iter()
                .map(|(mid, track_name)| PullTrack { mid, track_name })
                .collect();
            // The offer we just built must be answered before media flows.
            (offer_sdp, tracks, true)
        }
    };

    let active_mids: Vec<String> = tracks.iter().map(|track| track.mid.clone()).collect();
    if active_mids.is_empty()
        || !state.record_rtc_tracks(&session_id, &active_mids).await
        || require_live_session(&state, &claims, &session_id).await.is_err()
    {
        revoke_with_tracks(&state, &session_id, &active_mids).await;
        return Err(ApiError::forbidden("rtc websocket lease ended during pull"));
    }
    Ok(Json(PullResponse { offer_sdp, tracks, requires_renegotiation }))
}

#[derive(Deserialize)]
struct UnpullBody {
    /// Mids of previously pulled tracks the viewer stopped. The sdp is the
    /// browser's offer generated after stopping those transceivers.
    mids: Vec<String>,
    sdp: String,
}

#[derive(Serialize)]
struct UnpullResponse {
    answer_sdp: String,
}

/// Stop receiving previously pulled tracks so the SFU stops sending them —
/// the bandwidth-saving half of selective subscriptions.
async fn rtc_unpull(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<UnpullBody>,
) -> ApiResult<UnpullResponse> {
    let claims = require_auth(&state, &headers)?;
    require_live_session(&state, &claims, &session_id).await?;
    if body.mids.is_empty() || body.mids.len() > MAX_RTC_TRACKS_PER_SESSION {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "no mids to close"));
    }
    let answer_sdp = match state.cfg.rtc.backend {
        RtcBackend::Cloudflare => {
            realtime::close_tracks(&state, &session_id, &body.mids, &body.sdp).await?
        }
        RtcBackend::Builtin => {
            state.builtin_sfu().unsubscribe(&session_id, &body.mids, &body.sdp).await?
        }
    };
    state.forget_rtc_tracks(&session_id, &body.mids).await;
    Ok(Json(UnpullResponse { answer_sdp }))
}

#[derive(Deserialize)]
struct RenegotiateBody {
    sdp: String,
}

async fn rtc_renegotiate(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<RenegotiateBody>,
) -> ApiResult<serde_json::Value> {
    let claims = require_auth(&state, &headers)?;
    require_live_session(&state, &claims, &session_id).await?;
    match state.cfg.rtc.backend {
        RtcBackend::Cloudflare => realtime::renegotiate(&state, &session_id, &body.sdp).await?,
        RtcBackend::Builtin => state.builtin_sfu().accept_answer(&session_id, &body.sdp).await?,
    }
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AuthConfig, CloudflareConfig, Config, DiscordConfig, RtcConfig, ServerConfig,
    };
    use crate::state::Peer;
    use tokio::sync::mpsc;

    fn test_state() -> Arc<AppState> {
        Arc::new(AppState::new(
            Config {
                discord: DiscordConfig {
                    client_id: "client".into(),
                    client_secret: "secret".into(),
                    bot_token: "bot".into(),
                },
                cloudflare: Some(CloudflareConfig {
                    app_id: "app".into(),
                    app_secret: "secret".into(),
                }),
                rtc: RtcConfig::default(),
                server: ServerConfig { port: 0, public_domain: "example.test".into() },
                auth: AuthConfig { session_secret: "session-secret".into() },
            },
            reqwest::Client::new(),
            None,
        ))
    }

    #[test]
    fn debug_fields_are_bounded_and_control_characters_are_neutralized() {
        let clean = clean_log_field("hello\nforged\r\tentry", 13);
        assert_eq!(clean, "hello�forged�");
        assert!(!clean.chars().any(char::is_control));
    }

    #[test]
    fn public_debug_reports_are_rate_limited() {
        let state = test_state();
        for _ in 0..30 {
            assert!(state.allow_debug_report());
        }
        assert!(!state.allow_debug_report());
    }

    #[tokio::test]
    async fn rtc_session_cannot_move_to_another_socket_for_the_same_user() {
        let state = test_state();
        let claims = Claims {
            sub: "user".into(),
            name: "name".into(),
            avatar: None,
            inst: "room".into(),
            chan: None,
            guild: None,
            role: Role::Member,
            exp: u64::MAX,
        };
        let (tx, _rx) = mpsc::channel(1);
        state.rooms.write().await.entry("room".into()).or_default().peers.insert(
            10,
            Peer {
                user_id: "user".into(),
                role: Role::Member,
                tx,
                watching_video: HashSet::new(),
                watching_audio: HashSet::new(),
            },
        );
        let reservation = state
            .reserve_rtc_session(RtcSessionOwner {
                user_id: "user".into(),
                instance_id: "room".into(),
                role: Role::Member,
                conn_id: 10,
                expires_at: u64::MAX,
                tracks: HashSet::new(),
            })
            .await
            .unwrap();
        assert!(state.commit_rtc_session(&reservation, "session".into()).await);
        assert!(require_live_session(&state, &claims, "session").await.is_ok());

        let (replacement_tx, _replacement_rx) = mpsc::channel(1);
        let mut rooms = state.rooms.write().await;
        let room = rooms.get_mut("room").unwrap();
        room.peers.remove(&10);
        room.peers.insert(
            11,
            Peer {
                user_id: "user".into(),
                role: Role::Member,
                tx: replacement_tx,
                watching_video: HashSet::new(),
                watching_audio: HashSet::new(),
            },
        );
        drop(rooms);

        assert!(require_live_session(&state, &claims, "session").await.is_err());
    }
}
