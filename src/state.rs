use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::{mpsc, RwLock};

use crate::auth::Claims;
use crate::config::Config;
use crate::sfu::Sfu;

pub struct AppState {
    pub cfg: Config,
    pub http: reqwest::Client,
    /// The builtin SFU — present iff `rtc.backend = "builtin"`.
    pub sfu: Option<Arc<Sfu>>,
    pub rooms: RwLock<HashMap<String, Room>>,
    /// One-time codes minted in the activity, claimed by the external share page.
    pub share_codes: RwLock<HashMap<String, PendingCode>>,
    /// RTC session id -> owner, so publish/pull can't touch foreign sessions.
    /// Applies to both backends (Cloudflare session ids and builtin ones).
    pub rtc_sessions: RwLock<HashMap<String, RtcSessionOwner>>,
    pub next_conn_id: AtomicU64,
}

impl AppState {
    pub fn new(cfg: Config, http: reqwest::Client, sfu: Option<Arc<Sfu>>) -> Self {
        Self {
            cfg,
            http,
            sfu,
            rooms: RwLock::new(HashMap::new()),
            share_codes: RwLock::new(HashMap::new()),
            rtc_sessions: RwLock::new(HashMap::new()),
            next_conn_id: AtomicU64::new(1),
        }
    }

    /// Present iff `rtc.backend = "builtin"` — callers dispatch on the backend
    /// before reaching for this.
    pub fn builtin_sfu(&self) -> &Arc<Sfu> {
        self.sfu.as_ref().expect("builtin sfu constructed at startup")
    }

    /// Release backend resources tied to an ended share. Cloudflare garbage
    /// collects its own sessions; the builtin SFU is told explicitly.
    pub fn reap_share(&self, share: Option<Share>) {
        let (Some(share), Some(sfu)) = (share, &self.sfu) else { return };
        let sfu = sfu.clone();
        tokio::spawn(async move { sfu.close_session(&share.publisher_session_id).await });
    }
}

pub struct PendingCode {
    pub claims: Claims,
    pub expires_at: u64,
}

pub struct RtcSessionOwner {
    pub user_id: String,
    pub instance_id: String,
    /// Unix seconds; entries are pruned once older than the session TTL.
    pub created_at: u64,
}

#[derive(Default)]
pub struct Room {
    /// Connected websocket peers (viewers and publishers), keyed by connection id.
    pub peers: HashMap<u64, Peer>,
    /// Active screen shares, keyed by share id.
    pub shares: HashMap<String, Share>,
}

pub struct Peer {
    pub user_id: String,
    pub role: crate::auth::Role,
    pub tx: mpsc::UnboundedSender<String>,
}

#[derive(Clone, Serialize)]
pub struct Share {
    pub id: String,
    pub user_id: String,
    pub username: String,
    pub avatar: Option<String>,
    pub has_audio: bool,
    pub started_at: u64,
    /// Publisher's RTC session — needed server-side to build remote pulls
    /// (Cloudflare) and to tear the peer connection down (builtin).
    #[serde(skip)]
    pub publisher_session_id: String,
    /// Websocket connection that owns this share; share ends when it drops.
    #[serde(skip)]
    pub conn_id: u64,
}

impl Room {
    pub fn broadcast(&self, msg: &impl Serialize) {
        let raw = serde_json::to_string(msg).expect("broadcast serialize");
        for peer in self.peers.values() {
            let _ = peer.tx.send(raw.clone());
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg<'a> {
    /// Sent once to a peer right after websocket auth succeeds.
    Hello { conn_id: u64, shares: Vec<&'a Share> },
    ShareStarted { share: &'a Share },
    ShareEnded { share_id: &'a str },
}

impl Room {
    /// Remove a share and tell everyone. Returns the removed share so the
    /// caller can release backend resources via `AppState::reap_share`.
    pub fn end_share(&mut self, share_id: &str) -> Option<Share> {
        let share = self.shares.remove(share_id)?;
        self.broadcast(&ServerMsg::ShareEnded { share_id });
        Some(share)
    }
}
