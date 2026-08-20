use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{mpsc, Mutex, RwLock};

use crate::auth::Claims;
use crate::config::{Config, RtcBackend};
use crate::sfu::Sfu;

pub const MAX_RTC_SESSIONS_PER_USER: usize = 16;
pub const MAX_RTC_SESSIONS_TOTAL: usize = 512;
const MAX_RTC_CREATIONS_PER_USER_PER_MINUTE: usize = 32;
const MAX_RTC_CREATIONS_TOTAL_PER_MINUTE: usize = 1024;
const MAX_DEBUG_REPORTS_PER_MINUTE: u32 = 30;

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
    pub rtc_sessions: Mutex<RtcSessionRegistry>,
    debug_reports: StdMutex<DebugRateWindow>,
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
            rtc_sessions: Mutex::new(RtcSessionRegistry::default()),
            debug_reports: StdMutex::new(DebugRateWindow::default()),
            next_conn_id: AtomicU64::new(1),
        }
    }

    /// Present iff `rtc.backend = "builtin"` — callers dispatch on the backend
    /// before reaching for this.
    pub fn builtin_sfu(&self) -> &Arc<Sfu> {
        self.sfu.as_ref().expect("builtin sfu constructed at startup")
    }

    pub async fn reserve_rtc_session(
        self: &Arc<Self>,
        owner: RtcSessionOwner,
    ) -> Result<String, RtcLimitError> {
        self.prune_expired_rtc_sessions(crate::auth::now_secs()).await;
        self.rtc_sessions.lock().await.reserve(owner, crate::auth::now_secs())
    }

    pub async fn commit_rtc_session(&self, reservation_id: &str, session_id: String) -> bool {
        self.rtc_sessions.lock().await.commit(reservation_id, session_id)
    }

    pub async fn cancel_rtc_reservation(&self, reservation_id: &str) {
        self.rtc_sessions.lock().await.reservations.remove(reservation_id);
    }

    pub async fn rtc_session_owner(&self, session_id: &str) -> Option<RtcSessionOwner> {
        self.rtc_sessions.lock().await.sessions.get(session_id).cloned()
    }

    /// Add media mids to a live session. False means the owning websocket
    /// revoked the session while signaling was in flight.
    pub async fn record_rtc_tracks(&self, session_id: &str, mids: &[String]) -> bool {
        let mut registry = self.rtc_sessions.lock().await;
        let Some(owner) = registry.sessions.get_mut(session_id) else { return false };
        owner.tracks.extend(mids.iter().cloned());
        true
    }

    pub async fn forget_rtc_tracks(&self, session_id: &str, mids: &[String]) {
        let mut registry = self.rtc_sessions.lock().await;
        let Some(owner) = registry.sessions.get_mut(session_id) else { return };
        for mid in mids {
            owner.tracks.remove(mid);
        }
    }

    pub async fn revoke_rtc_session(self: &Arc<Self>, session_id: &str) {
        let owner = self.rtc_sessions.lock().await.sessions.remove(session_id);
        if let Some(owner) = owner {
            self.close_backend_session(session_id, &owner.tracks).await;
        }
    }

    /// Revoke every RTC allocation tied to one exact websocket lease.
    pub async fn revoke_connection(self: &Arc<Self>, instance_id: &str, conn_id: u64) {
        let sessions = {
            let mut registry = self.rtc_sessions.lock().await;
            registry.take_connection(instance_id, conn_id)
        };
        for (session_id, owner) in sessions {
            self.close_backend_session(&session_id, &owner.tracks).await;
        }
    }

    pub async fn close_backend_session(&self, session_id: &str, tracks: &HashSet<String>) {
        match self.cfg.rtc.backend {
            RtcBackend::Cloudflare => {
                if tracks.is_empty() {
                    return;
                }
                let mids: Vec<String> = tracks.iter().cloned().collect();
                for chunk in mids.chunks(64) {
                    for attempt in 1..=3 {
                        match crate::realtime::force_close_tracks(self, session_id, chunk).await {
                            Ok(()) => break,
                            Err(err) if attempt < 3 => {
                                tracing::warn!(session_id, attempt, "Cloudflare media revocation failed; retrying: {err:#}");
                                tokio::time::sleep(Duration::from_millis(100 * attempt)).await;
                            }
                            Err(err) => {
                                tracing::error!(session_id, "failed to revoke a Cloudflare media batch after retries: {err:#}");
                            }
                        }
                    }
                }
            }
            RtcBackend::Builtin => self.builtin_sfu().close_session(session_id).await,
        }
    }

    /// Release backend resources tied to an ended share without holding the
    /// room lock across a provider request.
    pub fn reap_share(self: &Arc<Self>, share: Option<Share>) {
        let Some(share) = share else { return };
        let state = self.clone();
        tokio::spawn(async move { state.revoke_rtc_session(&share.publisher_session_id).await });
    }

    pub fn allow_debug_report(&self) -> bool {
        let mut window = self.debug_reports.lock().unwrap_or_else(|e| e.into_inner());
        if window.started.elapsed() >= Duration::from_secs(60) {
            *window = DebugRateWindow::default();
        }
        if window.count >= MAX_DEBUG_REPORTS_PER_MINUTE {
            return false;
        }
        window.count += 1;
        true
    }

    async fn prune_expired_rtc_sessions(self: &Arc<Self>, now: u64) {
        let expired = self.rtc_sessions.lock().await.prune_expired(now);
        for (session_id, owner) in expired {
            let state = self.clone();
            tokio::spawn(async move {
                state.close_backend_session(&session_id, &owner.tracks).await;
            });
        }
    }
}

struct DebugRateWindow {
    started: Instant,
    count: u32,
}

impl Default for DebugRateWindow {
    fn default() -> Self {
        Self { started: Instant::now(), count: 0 }
    }
}

pub struct PendingCode {
    pub claims: Claims,
    pub expires_at: u64,
}

#[derive(Clone)]
pub struct RtcSessionOwner {
    pub user_id: String,
    pub instance_id: String,
    pub role: crate::auth::Role,
    /// Exact websocket lease that authorizes this media allocation.
    pub conn_id: u64,
    /// Unix seconds inherited from the signed session token.
    pub expires_at: u64,
    /// Local/remote media mids currently active in this backend session.
    pub tracks: HashSet<String>,
}

#[derive(Default)]
pub struct RtcSessionRegistry {
    sessions: HashMap<String, RtcSessionOwner>,
    reservations: HashMap<String, RtcSessionOwner>,
    recent_creations: VecDeque<(u64, String)>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RtcLimitError {
    PerUser,
    Total,
    PerUserRate,
    TotalRate,
}

impl RtcSessionRegistry {
    fn reserve(&mut self, owner: RtcSessionOwner, now: u64) -> Result<String, RtcLimitError> {
        let cutoff = now.saturating_sub(60);
        while self.recent_creations.front().is_some_and(|(created, _)| *created <= cutoff) {
            self.recent_creations.pop_front();
        }
        if self.recent_creations.len() >= MAX_RTC_CREATIONS_TOTAL_PER_MINUTE {
            return Err(RtcLimitError::TotalRate);
        }
        let recent_for_user =
            self.recent_creations.iter().filter(|(_, user)| user == &owner.user_id).count();
        if recent_for_user >= MAX_RTC_CREATIONS_PER_USER_PER_MINUTE {
            return Err(RtcLimitError::PerUserRate);
        }
        let total = self.sessions.len() + self.reservations.len();
        if total >= MAX_RTC_SESSIONS_TOTAL {
            return Err(RtcLimitError::Total);
        }
        let owned = self
            .sessions
            .values()
            .chain(self.reservations.values())
            .filter(|candidate| candidate.user_id == owner.user_id)
            .count();
        if owned >= MAX_RTC_SESSIONS_PER_USER {
            return Err(RtcLimitError::PerUser);
        }
        let reservation_id = crate::auth::random_id(16);
        self.recent_creations.push_back((now, owner.user_id.clone()));
        self.reservations.insert(reservation_id.clone(), owner);
        Ok(reservation_id)
    }

    fn commit(&mut self, reservation_id: &str, session_id: String) -> bool {
        let Some(owner) = self.reservations.remove(reservation_id) else { return false };
        self.sessions.insert(session_id, owner);
        true
    }

    fn prune_expired(&mut self, now: u64) -> Vec<(String, RtcSessionOwner)> {
        self.reservations.retain(|_, owner| owner.expires_at > now);
        let ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, owner)| owner.expires_at <= now)
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| self.sessions.remove(&id).map(|owner| (id, owner)))
            .collect()
    }

    fn take_connection(
        &mut self,
        instance_id: &str,
        conn_id: u64,
    ) -> Vec<(String, RtcSessionOwner)> {
        self.reservations
            .retain(|_, owner| owner.instance_id != instance_id || owner.conn_id != conn_id);
        let ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, owner)| owner.instance_id == instance_id && owner.conn_id == conn_id)
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| self.sessions.remove(&id).map(|owner| (id, owner)))
            .collect()
    }
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
    pub tx: mpsc::Sender<String>,
    /// Share ids whose video/audio this peer is (or intends to be) pulling.
    /// Drives the viewer counts publishers use to pause idle encoders.
    pub watching_video: std::collections::HashSet<String>,
    pub watching_audio: std::collections::HashSet<String>,
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
            let _ = peer.tx.try_send(raw.clone());
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
    /// Sent to a share's publisher whenever its audience may have changed, so
    /// it can stop encoding (and uploading) media nobody is subscribed to.
    /// `total` counts distinct peers subscribed to either kind.
    ViewerCount { share_id: &'a str, video: usize, audio: usize, total: usize },
}

impl Room {
    /// Remove a share and tell everyone. Returns the removed share so the
    /// caller can release backend resources via `AppState::reap_share`.
    pub fn end_share(&mut self, share_id: &str) -> Option<Share> {
        let share = self.shares.remove(share_id)?;
        self.broadcast(&ServerMsg::ShareEnded { share_id });
        Some(share)
    }

    /// Tell every publisher how many peers are subscribed to its share.
    /// Cheap enough (rooms are one voice channel) to recompute wholesale on
    /// any change instead of tracking deltas.
    pub fn push_viewer_counts(&self) {
        for share in self.shares.values() {
            let Some(publisher) = self.peers.get(&share.conn_id) else { continue };
            let (mut video, mut audio, mut total) = (0, 0, 0);
            for peer in self.peers.values() {
                let watches_video = peer.watching_video.contains(&share.id);
                let watches_audio = peer.watching_audio.contains(&share.id);
                video += usize::from(watches_video);
                audio += usize::from(watches_audio);
                total += usize::from(watches_video || watches_audio);
            }
            let msg = ServerMsg::ViewerCount { share_id: &share.id, video, audio, total };
            let raw = serde_json::to_string(&msg).expect("viewer count serialize");
            let _ = publisher.tx.try_send(raw);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Role;

    fn owner(user: &str, conn_id: u64) -> RtcSessionOwner {
        RtcSessionOwner {
            user_id: user.into(),
            instance_id: "room".into(),
            role: Role::Member,
            conn_id,
            expires_at: u64::MAX,
            tracks: HashSet::new(),
        }
    }

    #[test]
    fn reservations_make_session_caps_atomic() {
        let mut registry = RtcSessionRegistry::default();
        for _ in 0..MAX_RTC_SESSIONS_PER_USER {
            registry.reserve(owner("user", 1), 100).unwrap();
        }
        assert_eq!(registry.reserve(owner("user", 1), 100), Err(RtcLimitError::PerUser));
        assert!(registry.reserve(owner("another", 2), 100).is_ok());
    }

    #[test]
    fn pruning_removes_expired_sessions_and_reservations() {
        let mut registry = RtcSessionRegistry::default();
        let mut expired = owner("user", 1);
        expired.expires_at = 5;
        registry.sessions.insert("session".into(), expired.clone());
        registry.reservations.insert("reservation".into(), expired);

        let removed = registry.prune_expired(5);
        assert_eq!(removed.len(), 1);
        assert!(registry.sessions.is_empty());
        assert!(registry.reservations.is_empty());
    }

    #[test]
    fn websocket_revocation_only_takes_its_rtc_sessions() {
        let mut registry = RtcSessionRegistry::default();
        registry.sessions.insert("first".into(), owner("user", 10));
        registry.sessions.insert("second".into(), owner("user", 11));
        registry.reservations.insert("pending-first".into(), owner("user", 10));
        registry.reservations.insert("pending-second".into(), owner("user", 11));

        let revoked = registry.take_connection("room", 10);
        assert_eq!(revoked.len(), 1);
        assert_eq!(revoked[0].0, "first");
        assert!(!registry.reservations.contains_key("pending-first"));
        assert!(registry.sessions.contains_key("second"));
        assert!(registry.reservations.contains_key("pending-second"));
    }

    #[test]
    fn session_creation_churn_is_rate_limited() {
        let mut registry = RtcSessionRegistry::default();
        for index in 0..MAX_RTC_CREATIONS_PER_USER_PER_MINUTE {
            let reservation = registry.reserve(owner("user", index as u64), 100).unwrap();
            registry.reservations.remove(&reservation);
        }
        assert_eq!(
            registry.reserve(owner("user", 999), 100),
            Err(RtcLimitError::PerUserRate)
        );
        assert!(registry.reserve(owner("user", 999), 161).is_ok());
    }
}
