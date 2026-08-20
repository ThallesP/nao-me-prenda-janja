use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::{self, Claims, Role, SESSION_TTL_SECS};
use crate::config::RtcBackend;
use crate::state::{AppState, PendingCode, RtcSessionOwner, ServerMsg, Share};
use crate::{discord, realtime, ws};

const SHARE_CODE_TTL_SECS: u64 = 300;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/config-public", get(config_public))
        .route("/api/auth/token", post(auth_token))
        .route("/api/share-link", post(share_link))
        .route("/api/share/claim", post(share_claim))
        .route("/api/share/stop", post(share_stop))
        .route("/api/rtc/session", post(rtc_session_new))
        .route("/api/rtc/session/{id}/publish", post(rtc_publish))
        .route("/api/rtc/session/{id}/pull", post(rtc_pull))
        .route("/api/rtc/session/{id}/renegotiate", put(rtc_renegotiate))
        .route("/api/ws", get(ws::upgrade))
        .route("/api/debug", post(debug_report))
        .fallback(crate::assets::serve)
        .layer(middleware::from_fn(log_requests))
        .with_state(state)
}

/// Clients post here when the Discord handshake stalls — log-only diagnostics.
async fn debug_report(body: String) -> StatusCode {
    let trimmed: String = body.chars().take(2000).collect();
    tracing::warn!("client debug: {trimmed}");
    StatusCode::NO_CONTENT
}

async fn log_requests(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = next.run(req).await;
    if path.starts_with("/api") {
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
        .is_some_and(|room| room.peers.values().any(|p| p.user_id == claims.sub));
    if !alive {
        return Err(ApiError::forbidden("no active connection to this activity"));
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

async fn rtc_session_new(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<SessionNewResponse> {
    let claims = require_auth(&state, &headers)?;
    require_live_peer(&state, &claims).await?;
    let session_id = match state.cfg.rtc.backend {
        RtcBackend::Cloudflare => realtime::new_session(&state).await?,
        RtcBackend::Builtin => state.builtin_sfu().new_session().await?,
    };
    let now = auth::now_secs();
    let mut sessions = state.rtc_sessions.write().await;
    sessions.retain(|_, owner| owner.created_at + SESSION_TTL_SECS > now);
    sessions.insert(
        session_id.clone(),
        RtcSessionOwner { user_id: claims.sub, instance_id: claims.inst, created_at: now },
    );
    Ok(Json(SessionNewResponse { session_id }))
}

async fn check_session_owner(
    state: &AppState,
    claims: &Claims,
    session_id: &str,
) -> Result<(), ApiError> {
    let sessions = state.rtc_sessions.read().await;
    let owner = sessions
        .get(session_id)
        .ok_or_else(|| ApiError::not_found("unknown rtc session"))?;
    if owner.user_id != claims.sub || owner.instance_id != claims.inst {
        return Err(ApiError::forbidden("not your rtc session"));
    }
    Ok(())
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
    check_session_owner(&state, &claims, &session_id).await?;

    // The websocket connection that owns this share must exist, be ours, and
    // be the share page's Publisher socket (not the activity iframe's Member
    // socket) — the share is torn down when that socket drops.
    {
        let rooms = state.rooms.read().await;
        let valid_conn = rooms
            .get(&claims.inst)
            .and_then(|room| room.peers.get(&body.conn_id))
            .is_some_and(|peer| peer.user_id == claims.sub && peer.role == Role::Publisher);
        if !valid_conn {
            return Err(ApiError::forbidden("connect the share page websocket first"));
        }
    }

    let share_id = auth::random_id(8);
    // Track names are chosen server-side; clients never get to pick them.
    let mut tracks = vec![(body.video_mid.clone(), format!("{share_id}:v"))];
    if let Some(audio_mid) = &body.audio_mid {
        tracks.push((audio_mid.clone(), format!("{share_id}:a")));
    }
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
            let resp =
                realtime::publish_tracks(&state, &session_id, &body.sdp, &cf_tracks).await?;
            for track in &resp.tracks {
                if let Some(code) = &track.error_code {
                    tracing::error!(
                        "cloudflare track error: {code} {}",
                        track.error_description.as_deref().unwrap_or("")
                    );
                    return Err(ApiError::new(StatusCode::BAD_GATEWAY, "sfu rejected a track"));
                }
            }
            resp.session_description
                .ok_or_else(|| ApiError::new(StatusCode::BAD_GATEWAY, "sfu returned no answer"))?
                .sdp
        }
        RtcBackend::Builtin => state.builtin_sfu().publish(&session_id, &body.sdp, &tracks).await?,
    };

    let share = Share {
        id: share_id.clone(),
        user_id: claims.sub.clone(),
        username: claims.name.clone(),
        avatar: claims.avatar.clone(),
        has_audio: body.audio_mid.is_some(),
        started_at: auth::now_secs(),
        publisher_session_id: session_id,
        conn_id: body.conn_id,
    };

    let mut rooms = state.rooms.write().await;
    let room = rooms
        .get_mut(&claims.inst)
        .ok_or_else(|| ApiError::forbidden("room is gone"))?;
    // Re-check under the write lock: if the publisher websocket dropped while
    // we were talking to Cloudflare, inserting now would leak an ownerless
    // share (its cleanup already ran).
    let conn_alive = room
        .peers
        .get(&body.conn_id)
        .is_some_and(|peer| peer.user_id == claims.sub && peer.role == Role::Publisher);
    if !conn_alive {
        return Err(ApiError::forbidden("share page websocket disconnected"));
    }
    // One live share per user, Discord-style: starting a new one replaces the old.
    let previous: Vec<String> = room
        .shares
        .values()
        .filter(|s| s.user_id == claims.sub)
        .map(|s| s.id.clone())
        .collect();
    for id in previous {
        state.reap_share(room.end_share(&id));
    }
    room.broadcast(&ServerMsg::ShareStarted { share: &share });
    room.shares.insert(share_id.clone(), share);

    Ok(Json(PublishResponse { share_id, answer_sdp }))
}

#[derive(Deserialize)]
struct PullBody {
    share_id: String,
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
    require_live_peer(&state, &claims).await?;
    check_session_owner(&state, &claims, &session_id).await?;

    let (publisher_session_id, track_names) = {
        let rooms = state.rooms.read().await;
        let share = rooms
            .get(&claims.inst)
            .and_then(|room| room.shares.get(&body.share_id))
            .ok_or_else(|| ApiError::not_found("share not found"))?;
        let mut names = vec![format!("{}:v", share.id)];
        if share.has_audio {
            names.push(format!("{}:a", share.id));
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
            let resp = realtime::pull_tracks(&state, &session_id, &remote_tracks).await?;
            for track in &resp.tracks {
                if let Some(code) = &track.error_code {
                    tracing::error!(
                        "cloudflare pull track error: {code} {}",
                        track.error_description.as_deref().unwrap_or("")
                    );
                    return Err(ApiError::new(StatusCode::BAD_GATEWAY, "sfu rejected a pull"));
                }
            }
            let offer_sdp = resp
                .session_description
                .ok_or_else(|| ApiError::new(StatusCode::BAD_GATEWAY, "sfu returned no offer"))?
                .sdp;
            let requires_renegotiation = resp.requires_immediate_renegotiation;
            let tracks = resp
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
            let (offer_sdp, pairs) = state.builtin_sfu().subscribe(&session_id, &track_names).await?;
            let tracks = pairs
                .into_iter()
                .map(|(mid, track_name)| PullTrack { mid, track_name })
                .collect();
            // The offer we just built must be answered before media flows.
            (offer_sdp, tracks, true)
        }
    };
    Ok(Json(PullResponse { offer_sdp, tracks, requires_renegotiation }))
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
    check_session_owner(&state, &claims, &session_id).await?;
    match state.cfg.rtc.backend {
        RtcBackend::Cloudflare => realtime::renegotiate(&state, &session_id, &body.sdp).await?,
        RtcBackend::Builtin => state.builtin_sfu().accept_answer(&session_id, &body.sdp).await?,
    }
    Ok(Json(json!({ "ok": true })))
}
