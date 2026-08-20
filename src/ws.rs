use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::state::{AppState, Peer, ServerMsg};
use crate::{auth, discord};

/// How often to re-verify with Discord that the user is still in the
/// activity instance. Leaving the voice channel cuts access within this window.
const MEMBERSHIP_RECHECK_SECS: u64 = 300;

pub async fn upgrade(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle(state, socket))
}

#[derive(Deserialize)]
struct AuthMsg {
    token: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    StopShare { share_id: String },
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
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    let hello = {
        let mut rooms = state.rooms.write().await;
        let room = rooms.entry(claims.inst.clone()).or_default();
        room.peers.insert(
            conn_id,
            Peer { user_id: claims.sub.clone(), role: claims.role, tx },
        );
        serde_json::to_string(&ServerMsg::Hello {
            conn_id,
            shares: room.shares.values().collect(),
        })
        .expect("hello serialize")
    };
    if socket.send(Message::Text(hello.into())).await.is_err() {
        cleanup(&state, &claims.inst, conn_id).await;
        return;
    }
    tracing::debug!(user = %claims.sub, inst = %claims.inst, conn_id, "ws joined");

    let (mut sink, mut stream) = socket.split();
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut recheck = tokio::time::interval(Duration::from_secs(MEMBERSHIP_RECHECK_SECS));
    recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    recheck.tick().await; // consume the immediate first tick — we just verified

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
                    if let Ok(ClientMsg::StopShare { share_id }) =
                        serde_json::from_str(raw.as_str())
                    {
                        stop_share(&state, &claims.inst, conn_id, &share_id).await;
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
            _ = recheck.tick() => {
                // Kick only on a definitive "not a participant" — a transient
                // Discord API error shouldn't drop the whole room.
                let check =
                    discord::check_instance_participant(&state, &claims.inst, &claims.sub).await;
                if matches!(check, Ok(c) if !c.is_participant) {
                    tracing::debug!(user = %claims.sub, "left activity instance, closing ws");
                    break;
                }
            }
        }
    }

    cleanup(&state, &claims.inst, conn_id).await;
}

/// Explicit stop from the publisher page. Ownership is by connection, which
/// is stronger than by user: only the socket that registered the share may
/// stop it this way.
async fn stop_share(state: &AppState, instance_id: &str, conn_id: u64, share_id: &str) {
    let mut rooms = state.rooms.write().await;
    let Some(room) = rooms.get_mut(instance_id) else { return };
    let owned = room.shares.get(share_id).is_some_and(|s| s.conn_id == conn_id);
    if owned {
        state.reap_share(room.end_share(share_id));
    }
}

async fn cleanup(state: &AppState, instance_id: &str, conn_id: u64) {
    let mut rooms = state.rooms.write().await;
    let Some(room) = rooms.get_mut(instance_id) else { return };
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
    }
}
