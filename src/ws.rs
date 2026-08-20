use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use crate::state::{AppState, Peer, Room, ServerMsg};
use crate::{auth, discord};

/// How often to re-verify with Discord that the user is still in the
/// activity instance. Leaving the voice channel cuts access within this window.
const MEMBERSHIP_RECHECK_SECS: u64 = 300;
/// Transient Discord failures are retried quickly, but authorization is
/// revoked when the dependency has been unavailable for this long.
const MEMBERSHIP_ERROR_GRACE_SECS: u64 = 60;
const MEMBERSHIP_ERROR_RETRY_SECS: u64 = 30;
const OUTGOING_QUEUE_CAPACITY: usize = 64;
const MAX_WS_CONNECTIONS_PER_USER: usize = 8;
const MAX_WS_CONNECTIONS_PER_ROOM: usize = 128;
const MAX_WS_CONNECTIONS_TOTAL: usize = 512;
const MAX_WATCHING_IDS: usize = 128;
const MAX_WS_MESSAGE_BYTES: usize = 64 * 1024;

pub async fn upgrade(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle(state, socket))
}

#[derive(Deserialize)]
struct AuthMsg {
    token: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    StopShare { share_id: String },
    /// Full replacement of the share ids this viewer is subscribed to (sent
    /// on intent, before the pull completes, so a paused publisher resumes
    /// encoding in time for the pull to succeed).
    Watching { video: Vec<String>, audio: Vec<String> },
}

async fn handle(state: Arc<AppState>, mut socket: WebSocket) {
    // Browsers can't set headers on websockets, so auth is the first message.
    let first = tokio::time::timeout(Duration::from_secs(10), socket.recv()).await;
    let Ok(Some(Ok(Message::Text(raw)))) = first else { return };
    let Ok(AuthMsg { token }) = serde_json::from_str::<AuthMsg>(raw.as_str()) else { return };
    let Some(claims) = auth::verify(&state.cfg.auth.session_secret, &token) else {
        let _ = socket.send(Message::Text(r#"{"type":"auth_failed"}"#.into())).await;
        return;
    };

    // Session tokens outlive channel membership — re-check with Discord that
    // this user is still actually in the activity instance before joining.
    let still_in = discord::check_instance_participant(&state, &claims.inst, &claims.sub)
        .await
        .map(|check| check.is_participant)
        .unwrap_or(false);
    if !still_in {
        let _ = socket.send(Message::Text(r#"{"type":"auth_failed"}"#.into())).await;
        return;
    }

    let conn_id = state.next_conn_id.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::channel::<String>(OUTGOING_QUEUE_CAPACITY);

    let hello_result = {
        let mut rooms = state.rooms.write().await;
        let total = rooms.values().map(|room| room.peers.len()).sum::<usize>();
        let user_total = rooms
            .values()
            .flat_map(|room| room.peers.values())
            .filter(|peer| peer.user_id == claims.sub)
            .count();
        let room_total = rooms.get(&claims.inst).map_or(0, |room| room.peers.len());
        if total >= MAX_WS_CONNECTIONS_TOTAL {
            Err("server connection capacity reached")
        } else if room_total >= MAX_WS_CONNECTIONS_PER_ROOM {
            Err("activity connection capacity reached")
        } else if user_total >= MAX_WS_CONNECTIONS_PER_USER {
            Err("too many connections for this user")
        } else {
            let room = rooms.entry(claims.inst.clone()).or_default();
            room.peers.insert(
                conn_id,
                Peer {
                    user_id: claims.sub.clone(),
                    role: claims.role,
                    tx,
                    watching_video: Default::default(),
                    watching_audio: Default::default(),
                },
            );
            Ok(serde_json::to_string(&ServerMsg::Hello {
                conn_id,
                shares: room.shares.values().collect(),
            })
            .expect("hello serialize"))
        }
    };
    let hello = match hello_result {
        Ok(hello) => hello,
        Err(reason) => {
            let raw = json!({ "type": "connection_rejected", "reason": reason }).to_string();
            let _ = socket.send(Message::Text(raw.into())).await;
            return;
        }
    };
    if socket.send(Message::Text(hello.into())).await.is_err() {
        cleanup(&state, &claims.inst, conn_id).await;
        return;
    }
    tracing::debug!(user = %claims.sub, inst = %claims.inst, conn_id, "ws joined");

    let (mut sink, mut stream) = socket.split();
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut membership_tick =
        tokio::time::interval(Duration::from_secs(MEMBERSHIP_ERROR_RETRY_SECS));
    membership_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    membership_tick.tick().await;
    let mut next_membership_check =
        tokio::time::Instant::now() + Duration::from_secs(MEMBERSHIP_RECHECK_SECS);
    let mut first_membership_error = None;
    let expires_in = claims.exp.saturating_sub(auth::now_secs());
    let token_expiry = tokio::time::sleep(Duration::from_secs(expires_in));
    tokio::pin!(token_expiry);

    loop {
        tokio::select! {
            outgoing = rx.recv() => match outgoing {
                Some(raw) => {
                    if sink.send(Message::Text(raw.into())).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(Message::Text(raw))) => {
                    match serde_json::from_str(raw.as_str()) {
                        Ok(ClientMsg::StopShare { share_id }) => {
                            stop_share(&state, &claims.inst, conn_id, &share_id).await;
                        }
                        Ok(ClientMsg::Watching { video, audio }) => {
                            if !set_watching(&state, &claims.inst, conn_id, video, audio).await {
                                tracing::debug!(user = %claims.sub, "rejected invalid watching state");
                            }
                        }
                        Err(_) => {}
                    }
                }
                Some(Ok(_)) => {}
            },
            _ = ping.tick() => {
                // Keepalive so idle reverse-proxy timeouts don't kill viewers.
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            _ = membership_tick.tick() => {
                let now = tokio::time::Instant::now();
                if now < next_membership_check {
                    continue;
                }
                let check =
                    discord::check_instance_participant(&state, &claims.inst, &claims.sub).await;
                match check {
                    Ok(check) if check.is_participant => {
                        first_membership_error = None;
                        next_membership_check =
                            now + Duration::from_secs(MEMBERSHIP_RECHECK_SECS);
                    }
                    Ok(_) => {
                        tracing::debug!(user = %claims.sub, "left activity instance, closing ws");
                        break;
                    }
                    Err(err) => {
                        if membership_error_grace_exhausted(&mut first_membership_error, now) {
                            tracing::warn!(user = %claims.sub, "membership recheck unavailable; revoking websocket: {err:#}");
                            break;
                        }
                        tracing::warn!(user = %claims.sub, "membership recheck failed; retrying: {err:#}");
                        next_membership_check =
                            now + Duration::from_secs(MEMBERSHIP_ERROR_RETRY_SECS);
                    }
                }
            }
            _ = &mut token_expiry => {
                tracing::debug!(user = %claims.sub, "session token expired, closing ws");
                break;
            }
        }
    }

    cleanup(&state, &claims.inst, conn_id).await;
}

fn membership_error_grace_exhausted(
    first_error: &mut Option<tokio::time::Instant>,
    now: tokio::time::Instant,
) -> bool {
    let first_error = first_error.get_or_insert(now);
    now.duration_since(*first_error) >= Duration::from_secs(MEMBERSHIP_ERROR_GRACE_SECS)
}

/// Explicit stop from the publisher page. Ownership is by connection, which
/// is stronger than by user: only the socket that registered the share may
/// stop it this way.
async fn stop_share(state: &Arc<AppState>, instance_id: &str, conn_id: u64, share_id: &str) {
    let mut rooms = state.rooms.write().await;
    let Some(room) = rooms.get_mut(instance_id) else { return };
    let owned = room.shares.get(share_id).is_some_and(|s| s.conn_id == conn_id);
    if owned {
        state.reap_share(room.end_share(share_id));
    }
}

/// Replace a viewer's subscription sets and re-notify publishers.
async fn set_watching(
    state: &AppState,
    instance_id: &str,
    conn_id: u64,
    video: Vec<String>,
    audio: Vec<String>,
) -> bool {
    let mut rooms = state.rooms.write().await;
    let Some(room) = rooms.get_mut(instance_id) else { return false };
    let Some((video, audio)) = validate_watching(room, video, audio) else { return false };
    let Some(peer) = room.peers.get_mut(&conn_id) else { return false };
    peer.watching_video = video;
    peer.watching_audio = audio;
    room.push_viewer_counts();
    true
}

fn validate_watching(
    room: &Room,
    video: Vec<String>,
    audio: Vec<String>,
) -> Option<(HashSet<String>, HashSet<String>)> {
    if video.len() > MAX_WATCHING_IDS
        || audio.len() > MAX_WATCHING_IDS
        || video.iter().chain(&audio).any(|id| id.len() > 64)
        || video.iter().chain(&audio).any(|id| !room.shares.contains_key(id))
    {
        return None;
    }
    let video = video.into_iter().collect();
    let audio = audio.into_iter().collect();
    Some((video, audio))
}

async fn cleanup(state: &Arc<AppState>, instance_id: &str, conn_id: u64) {
    {
        let mut rooms = state.rooms.write().await;
        let Some(room) = rooms.get_mut(instance_id) else {
            drop(rooms);
            state.revoke_connection(instance_id, conn_id).await;
            return;
        };
        room.peers.remove(&conn_id);
        // A publisher's shares die with its connection.
        let owned: Vec<String> = room
            .shares
            .values()
            .filter(|s| s.conn_id == conn_id)
            .map(|s| s.id.clone())
            .collect();
        for id in owned {
            state.reap_share(room.end_share(&id));
        }
        if room.peers.is_empty() && room.shares.is_empty() {
            rooms.remove(instance_id);
        } else {
            // A departed viewer may have been someone's last watcher.
            room.push_viewer_counts();
        }
    }
    state.revoke_connection(instance_id, conn_id).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Share;

    fn room_with_share() -> Room {
        let mut room = Room::default();
        room.shares.insert(
            "share".into(),
            Share {
                id: "share".into(),
                user_id: "user".into(),
                username: "name".into(),
                avatar: None,
                has_audio: true,
                started_at: 1,
                publisher_session_id: "session".into(),
                conn_id: 1,
            },
        );
        room
    }

    #[test]
    fn watching_state_accepts_only_bounded_live_share_ids() {
        let room = room_with_share();
        assert!(validate_watching(&room, vec!["share".into()], vec!["share".into()]).is_some());
        assert!(validate_watching(&room, vec!["unknown".into()], vec![]).is_none());
        assert!(validate_watching(&room, vec!["share".into(); MAX_WATCHING_IDS + 1], vec![])
            .is_none());
    }

    #[test]
    fn membership_dependency_errors_fail_closed_after_grace() {
        let start = tokio::time::Instant::now();
        let mut first_error = None;
        assert!(!membership_error_grace_exhausted(&mut first_error, start));
        assert!(!membership_error_grace_exhausted(
            &mut first_error,
            start + Duration::from_secs(MEMBERSHIP_ERROR_GRACE_SECS - 1)
        ));
        assert!(membership_error_grace_exhausted(
            &mut first_error,
            start + Duration::from_secs(MEMBERSHIP_ERROR_GRACE_SECS)
        ));
    }
}
