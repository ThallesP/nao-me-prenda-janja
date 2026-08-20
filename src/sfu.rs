//! Built-in SFU: this binary relays WebRTC media itself instead of Cloudflare.
//!
//! Topology mirrors the Cloudflare Realtime flow so the frontend is identical:
//! publishers send an SDP offer we answer; viewers get server-initiated offers
//! (one per pull) they answer via renegotiate. RTP is forwarded untranscoded
//! from each publisher track to every subscribed viewer connection.
//!
//! All peer connections share one UDP socket (ICE ufrag demux), so a single
//! open firewall port carries every stream.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::sync::{Mutex, RwLock};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_AV1};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::ice::udp_mux::{UDPMux, UDPMuxDefault, UDPMuxParams};
use webrtc::ice::udp_network::UDPNetwork;
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::{RTCPFeedback, RTCRtpTransceiverInit};
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::{TrackLocal, TrackLocalWriter};
use webrtc::track::track_remote::TrackRemote;

use crate::auth;
use crate::config::BuiltinRtcConfig;

/// How long a pull waits for the publisher's first RTP to register the track.
const TRACK_WAIT: Duration = Duration::from_secs(3);
/// Sessions stuck before connecting (browser gave up mid-handshake) are
/// reaped after this long. Browsers negotiate immediately after creating a
/// session, so anything not Connected within this window is dead.
const STALE_SESSION_SECS: u64 = 120;
/// Peer connections are real local resources (unlike Cloudflare sessions), so
/// cap what authenticated users can allocate. A user legitimately holds one
/// publisher + one viewer session, plus a few transient retries.
const MAX_SESSIONS_PER_USER: usize = 16;
const MAX_SESSIONS_TOTAL: usize = 512;

pub struct Sfu {
    /// One UDP socket shared by every peer connection (ICE ufrag demux).
    udp_mux: Arc<dyn UDPMux + Send + Sync>,
    public_ip: String,
    /// Gather candidates from this local IP only. With the mux, all host
    /// candidates share one socket, and webrtc-ice closes "duplicate"
    /// candidates INCLUDING that shared socket — which is exactly what the
    /// 1:1 NAT mapping produces on a multi-interface machine. One local IP
    /// means one candidate, so the dedup path never fires.
    local_ip: Option<std::net::IpAddr>,
    /// Makes the limit check and insertion one allocation transaction.
    allocation_lock: Mutex<()>,
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    /// Live published tracks by server-assigned track name ("<share>:v"/":a").
    tracks: RwLock<HashMap<String, Arc<PublishedTrack>>>,
}

struct Session {
    pc: Arc<RTCPeerConnection>,
    /// Discord user this session was minted for (for per-user caps).
    owner: String,
    /// mid -> track name for tracks this session publishes; consulted when
    /// media arrives to name the incoming track. Empty for viewer sessions.
    published: RwLock<HashMap<String, String>>,
    /// Track names this (viewer) session already pulled — a repeat pull would
    /// silently duplicate media and egress.
    subscribed: Mutex<std::collections::HashSet<String>>,
    /// Serializes offer/answer rounds — concurrent signaling on one
    /// connection races inside webrtc-rs.
    signaling: Mutex<()>,
    created_at: u64,
}

struct PublishedTrack {
    local: Arc<TrackLocalStaticRTP>,
    /// Weak: the publisher's session owns the connection; a dead publisher
    /// must not be kept alive by viewers still holding the track.
    publisher_pc: Weak<RTCPeerConnection>,
    media_ssrc: u32,
}

impl PublishedTrack {
    /// Ask the publisher's encoder for a keyframe (new viewers, loss recovery).
    async fn request_keyframe(&self) {
        let Some(pc) = self.publisher_pc.upgrade() else { return };
        let pli = PictureLossIndication { sender_ssrc: 0, media_ssrc: self.media_ssrc };
        if let Err(err) = pc.write_rtcp(&[Box::new(pli)]).await {
            tracing::debug!("keyframe request failed: {err}");
        }
    }
}

impl Sfu {
    pub async fn new(cfg: &BuiltinRtcConfig, http: &reqwest::Client) -> Result<Arc<Self>> {
        let public_ip = match &cfg.public_ip {
            Some(ip) => ip.clone(),
            None => detect_public_ip(http).await?,
        };
        public_ip
            .parse::<std::net::IpAddr>()
            .with_context(|| format!("public_ip {public_ip:?} is not an IP address"))?;

        let socket = tokio::net::UdpSocket::bind(("0.0.0.0", cfg.udp_port))
            .await
            .with_context(|| format!("binding SFU media socket udp/{}", cfg.udp_port))?;
        tracing::info!(
            "builtin SFU: media on udp/{} (open it in the firewall), advertising {public_ip}",
            cfg.udp_port
        );

        let local_ip = default_route_ip().await;
        if local_ip.is_none() {
            tracing::warn!("builtin SFU: couldn't determine the default-route local IP; multi-interface machines may fail to gather candidates");
        }

        let sfu = Arc::new(Self {
            udp_mux: UDPMuxDefault::new(UDPMuxParams::new(socket)),
            public_ip,
            local_ip,
            allocation_lock: Mutex::new(()),
            sessions: RwLock::new(HashMap::new()),
            tracks: RwLock::new(HashMap::new()),
        });

        // Backstop sweep: the state-change handler reaps failed connections,
        // but sessions abandoned before ever connecting need a timer.
        let weak = Arc::downgrade(&sfu);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                let Some(sfu) = weak.upgrade() else { return };
                sfu.sweep_stale_sessions().await;
            }
        });

        Ok(sfu)
    }

    pub async fn new_session(self: &Arc<Self>, owner: &str) -> Result<String> {
        let _allocation = self.allocation_lock.lock().await;
        {
            let sessions = self.sessions.read().await;
            if sessions.len() >= MAX_SESSIONS_TOTAL {
                return Err(anyhow!("sfu session limit reached"));
            }
            let owned = sessions.values().filter(|s| s.owner == owner).count();
            if owned >= MAX_SESSIONS_PER_USER {
                return Err(anyhow!("per-user sfu session limit reached"));
            }
        }
        let session_id = auth::random_id(16);
        // MediaEngine and the interceptor registry latch per-connection
        // negotiated state — they must be fresh for every connection. Only
        // the UDP mux (and thus the port) is shared.
        let mut media = MediaEngine::default();
        media.register_default_codecs().map_err(|e| anyhow!("register codecs: {e}"))?;
        register_av1(&mut media)?;
        let registry = register_default_interceptors(Registry::new(), &mut media)
            .map_err(|e| anyhow!("register interceptors: {e}"))?;
        let mut setting = SettingEngine::default();
        setting.set_udp_network(UDPNetwork::Muxed(self.udp_mux.clone()));
        setting.set_nat_1to1_ips(vec![self.public_ip.clone()], RTCIceCandidateType::Host);
        if let Some(local_ip) = self.local_ip {
            setting.set_ip_filter(Box::new(move |ip| ip == local_ip));
        }
        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(setting)
            .build();
        let pc = Arc::new(
            api.new_peer_connection(RTCConfiguration::default())
                .await
                .map_err(|e| anyhow!("new peer connection: {e}"))?,
        );
        let session = Arc::new(Session {
            pc: pc.clone(),
            owner: owner.to_string(),
            published: RwLock::new(HashMap::new()),
            subscribed: Mutex::new(std::collections::HashSet::new()),
            signaling: Mutex::new(()),
            created_at: auth::now_secs(),
        });

        // Reap on terminal states — covers browsers that just vanish.
        {
            let sfu = Arc::downgrade(self);
            let sid = session_id.clone();
            pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
                let sfu = sfu.clone();
                let sid = sid.clone();
                Box::pin(async move {
                    if matches!(
                        state,
                        RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
                    ) {
                        if let Some(sfu) = sfu.upgrade() {
                            // Spawned, not awaited: pc.close() re-fires this
                            // handler (Failed -> Closed) whose mutex the
                            // current invocation still holds — awaiting close
                            // here deadlocks and leaks the whole connection.
                            tokio::spawn(async move { sfu.close_session(&sid).await });
                        }
                    }
                })
            }));
        }

        // Incoming media (publisher sessions only): name the track via the
        // mid map filled in by publish(), then fan RTP out to viewers.
        {
            let sfu = Arc::downgrade(self);
            let weak_session = Arc::downgrade(&session);
            pc.on_track(Box::new(move |track, _receiver, transceiver| {
                let sfu = sfu.clone();
                let weak_session = weak_session.clone();
                Box::pin(async move {
                    let (Some(sfu), Some(session)) = (sfu.upgrade(), weak_session.upgrade())
                    else {
                        return;
                    };
                    let mid = transceiver.mid().map(|m| m.to_string()).unwrap_or_default();
                    let Some(track_name) = session.published.read().await.get(&mid).cloned()
                    else {
                        tracing::warn!("sfu: media for unmapped mid {mid:?}, ignoring");
                        return;
                    };
                    tokio::spawn(async move {
                        sfu.forward_published_track(&session, track, track_name).await;
                    });
                })
            }));
        }

        self.sessions.write().await.insert(session_id.clone(), session);
        Ok(session_id)
    }

    /// Publisher leg: apply the browser's offer, remember which mid carries
    /// which server-assigned track name, and answer.
    pub async fn publish(
        &self,
        session_id: &str,
        offer_sdp: &str,
        tracks: &[(String, String)],
    ) -> Result<String> {
        let session = self.get_session(session_id).await?;
        // A mid that isn't in the offer would leave a share nothing can ever
        // pull — the incoming track would never map to a name.
        for (mid, _) in tracks {
            if !offer_sdp.lines().any(|l| l.trim() == format!("a=mid:{mid}")) {
                return Err(anyhow!("mid {mid} not present in the offer"));
            }
        }
        {
            let mut published = session.published.write().await;
            for (mid, track_name) in tracks {
                published.insert(mid.clone(), track_name.clone());
            }
        }

        let _signaling = session.signaling.lock().await;
        let offer = RTCSessionDescription::offer(offer_sdp.to_string())
            .map_err(|e| anyhow!("bad offer sdp: {e}"))?;
        session.pc.set_remote_description(offer).await.map_err(|e| anyhow!("set offer: {e}"))?;
        let answer =
            session.pc.create_answer(None).await.map_err(|e| anyhow!("create answer: {e}"))?;
        let mut gathered = session.pc.gathering_complete_promise().await;
        session.pc.set_local_description(answer).await.map_err(|e| anyhow!("set answer: {e}"))?;
        let _ = gathered.recv().await;
        let local = session
            .pc
            .local_description()
            .await
            .ok_or_else(|| anyhow!("no local description after publish"))?;
        // The answer echoes the offer's opus fmtp, which lacks stereo — and
        // Chrome only encodes stereo when the answer asks for it. Munge only
        // what the browser receives (set_local_description byte-compares
        // against the generated answer, so it can't be munged upstream; the
        // server side ignores opus fmtp when forwarding anyway).
        let mut sdp = local.sdp;
        if !sdp.contains("stereo=1") {
            sdp = sdp.replace("useinbandfec=1", "useinbandfec=1;stereo=1;sprop-stereo=1");
        }
        Ok(sdp)
    }

    /// Viewer leg: attach the published tracks to this session's connection
    /// and build the offer the browser must answer. Returns (offer sdp,
    /// [(mid, track name)]).
    pub async fn subscribe(
        &self,
        session_id: &str,
        track_names: &[String],
    ) -> Result<(String, Vec<(String, String)>)> {
        let session = self.get_session(session_id).await?;
        let _signaling = session.signaling.lock().await;

        {
            let subscribed = session.subscribed.lock().await;
            if let Some(dup) = track_names.iter().find(|n| subscribed.contains(*n)) {
                return Err(anyhow!("track {dup} already pulled into this session"));
            }
        }

        // Resolve every track before touching the connection — a failure
        // mid-add would leave orphan transceivers that duplicate media once
        // the frontend's retry succeeds.
        let mut resolved = Vec::with_capacity(track_names.len());
        for name in track_names {
            resolved.push((self.wait_for_track(name).await?, name.clone()));
        }

        let mut added: Vec<(Arc<webrtc::rtp_transceiver::RTCRtpTransceiver>, String)> = Vec::new();
        for (published, name) in resolved {
            let transceiver = session
                .pc
                .add_transceiver_from_track(
                    published.local.clone() as Arc<dyn TrackLocal + Send + Sync>,
                    Some(RTCRtpTransceiverInit {
                        direction: RTCRtpTransceiverDirection::Sendonly,
                        send_encodings: vec![],
                    }),
                )
                .await;
            let transceiver = match transceiver {
                Ok(t) => t,
                Err(err) => {
                    for (t, _) in &added {
                        let _ = session.pc.remove_track(&t.sender().await).await;
                    }
                    return Err(anyhow!("add track {name}: {err}"));
                }
            };
            spawn_viewer_rtcp_relay(transceiver.sender().await, published.clone());
            // New viewers need an immediate keyframe (video only — PLI is
            // meaningless for audio).
            if name.ends_with(":v") {
                published.request_keyframe().await;
            }
            added.push((transceiver, name));
        }

        let offer =
            session.pc.create_offer(None).await.map_err(|e| anyhow!("create offer: {e}"))?;
        let mut gathered = session.pc.gathering_complete_promise().await;
        session.pc.set_local_description(offer).await.map_err(|e| anyhow!("set offer: {e}"))?;
        let _ = gathered.recv().await;
        let local = session
            .pc
            .local_description()
            .await
            .ok_or_else(|| anyhow!("no local description after subscribe"))?;

        // Mids are assigned during offer creation.
        let mut pairs = Vec::with_capacity(added.len());
        for (transceiver, name) in added {
            let mid = transceiver
                .mid()
                .map(|m| m.to_string())
                .ok_or_else(|| anyhow!("no mid assigned for pulled track {name}"))?;
            pairs.push((mid, name));
        }
        let mut subscribed = session.subscribed.lock().await;
        for (_, name) in &pairs {
            subscribed.insert(name.clone());
        }
        Ok((local.sdp, pairs))
    }

    /// Viewer leg: stop forwarding the given mids. The browser stops its
    /// transceivers and sends the resulting offer; we drop our senders and
    /// answer, freeing the m-lines and the forwarding egress.
    pub async fn unsubscribe(
        &self,
        session_id: &str,
        mids: &[String],
        offer_sdp: &str,
    ) -> Result<String> {
        let session = self.get_session(session_id).await?;
        let _signaling = session.signaling.lock().await;

        let mut removed_names = Vec::new();
        for transceiver in session.pc.get_transceivers().await {
            let Some(mid) = transceiver.mid() else { continue };
            if !mids.iter().any(|m| *m == mid) {
                continue;
            }
            let sender = transceiver.sender().await;
            if let Some(track) = sender.track().await {
                removed_names.push(track.id().to_string());
            }
            if let Err(err) = session.pc.remove_track(&sender).await {
                tracing::debug!("sfu: remove_track mid {mid}: {err}");
            }
        }

        let offer = RTCSessionDescription::offer(offer_sdp.to_string())
            .map_err(|e| anyhow!("bad offer sdp: {e}"))?;
        session.pc.set_remote_description(offer).await.map_err(|e| anyhow!("set offer: {e}"))?;
        let answer =
            session.pc.create_answer(None).await.map_err(|e| anyhow!("create answer: {e}"))?;
        let mut gathered = session.pc.gathering_complete_promise().await;
        session.pc.set_local_description(answer).await.map_err(|e| anyhow!("set answer: {e}"))?;
        let _ = gathered.recv().await;
        let local = session
            .pc
            .local_description()
            .await
            .ok_or_else(|| anyhow!("no local description after unsubscribe"))?;
        // Only now may these names be pulled again without duplicating media.
        let mut subscribed = session.subscribed.lock().await;
        for name in removed_names {
            subscribed.remove(&name);
        }
        Ok(local.sdp)
    }

    /// Viewer answers the offer built by `subscribe`.
    pub async fn accept_answer(&self, session_id: &str, answer_sdp: &str) -> Result<()> {
        let session = self.get_session(session_id).await?;
        let _signaling = session.signaling.lock().await;
        let answer = RTCSessionDescription::answer(answer_sdp.to_string())
            .map_err(|e| anyhow!("bad answer sdp: {e}"))?;
        session
            .pc
            .set_remote_description(answer)
            .await
            .map_err(|e| anyhow!("set answer: {e}"))?;
        Ok(())
    }

    pub async fn close_session(&self, session_id: &str) {
        let removed = self.sessions.write().await.remove(session_id);
        if let Some(session) = removed {
            if let Err(err) = session.pc.close().await {
                tracing::debug!("closing sfu session {session_id}: {err}");
            }
        }
    }

    async fn get_session(&self, session_id: &str) -> Result<Arc<Session>> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown sfu session {session_id}"))
    }

    /// A pull can race the publisher's first RTP packet (the track registers
    /// only once media arrives) — wait briefly instead of failing the pull.
    async fn wait_for_track(&self, name: &str) -> Result<Arc<PublishedTrack>> {
        let deadline = tokio::time::Instant::now() + TRACK_WAIT;
        loop {
            if let Some(track) = self.tracks.read().await.get(name).cloned() {
                return Ok(track);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow!("published track {name} not available"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Read RTP from a publisher and fan it out to every subscribed viewer.
    /// Runs until the publisher connection dies.
    async fn forward_published_track(
        &self,
        session: &Session,
        track: Arc<TrackRemote>,
        track_name: String,
    ) {
        let codec = track.codec();
        // One stream id per share so a viewer pulling video + audio groups them.
        let share_id = track_name.split(':').next().unwrap_or(&track_name);
        let local = Arc::new(TrackLocalStaticRTP::new(
            codec.capability,
            track_name.clone(),
            format!("janja-{share_id}"),
        ));
        let published = Arc::new(PublishedTrack {
            local: local.clone(),
            publisher_pc: Arc::downgrade(&session.pc),
            media_ssrc: track.ssrc(),
        });
        self.tracks.write().await.insert(track_name.clone(), published);
        tracing::info!("sfu: publishing track {track_name}");

        loop {
            match track.read_rtp().await {
                Ok((mut packet, _)) => {
                    // Header extension ids are negotiated per connection; the
                    // publisher's ids are meaningless (or harmful) on viewer
                    // legs, so strip them rather than forward mismatched ones.
                    packet.header.extension = false;
                    packet.header.extension_profile = 0;
                    packet.header.extensions.clear();
                    if let Err(err) = local.write_rtp(&packet).await {
                        // Individual viewer write errors (mid-teardown) are
                        // normal; keep serving the healthy viewers.
                        tracing::trace!("sfu: write_rtp {track_name}: {err}");
                    }
                }
                Err(_) => break, // publisher gone
            }
        }

        self.tracks.write().await.remove(&track_name);
        tracing::info!("sfu: track {track_name} ended");
    }

    async fn sweep_stale_sessions(&self) {
        let now = auth::now_secs();
        let stale: Vec<String> = {
            let sessions = self.sessions.read().await;
            let mut ids = Vec::new();
            for (id, session) in sessions.iter() {
                let state = session.pc.connection_state();
                let terminal = matches!(
                    state,
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
                );
                let never_connected = matches!(
                    state,
                    RTCPeerConnectionState::New | RTCPeerConnectionState::Connecting
                ) && session.created_at + STALE_SESSION_SECS < now;
                if terminal || never_connected {
                    ids.push(id.clone());
                }
            }
            ids
        };
        for id in stale {
            tracing::debug!("sfu: sweeping stale session {id}");
            self.close_session(&id).await;
        }
    }
}

/// webrtc-rs's default codec set stops at VP9, but browsers can encode AV1,
/// whose screen-content tools roughly halve screen-share bitrate. Register it
/// so publisher and viewer legs can negotiate it; forwarding is codec-blind.
fn register_av1(media: &mut MediaEngine) -> Result<()> {
    let video_rtcp_feedback = vec![
        RTCPFeedback { typ: "goog-remb".to_owned(), parameter: "".to_owned() },
        RTCPFeedback { typ: "ccm".to_owned(), parameter: "fir".to_owned() },
        RTCPFeedback { typ: "nack".to_owned(), parameter: "".to_owned() },
        RTCPFeedback { typ: "nack".to_owned(), parameter: "pli".to_owned() },
    ];
    media
        .register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_AV1.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: "".to_owned(),
                    rtcp_feedback: video_rtcp_feedback,
                },
                payload_type: 45,
                ..Default::default()
            },
            RTPCodecType::Video,
        )
        .map_err(|e| anyhow!("register av1: {e}"))
}

/// Forward viewer keyframe requests (PLI/FIR) to the publisher; also drains
/// the sender's RTCP so the interceptor chain (NACK responder etc.) runs.
fn spawn_viewer_rtcp_relay(sender: Arc<RTCRtpSender>, published: Arc<PublishedTrack>) {
    tokio::spawn(async move {
        loop {
            let packets = match sender.read_rtcp().await {
                Ok((packets, _)) => packets,
                Err(_) => return, // sender closed / track removed
            };
            for packet in packets {
                let any = packet.as_any();
                if any.downcast_ref::<PictureLossIndication>().is_some()
                    || any.downcast_ref::<FullIntraRequest>().is_some()
                {
                    published.request_keyframe().await;
                }
            }
        }
    });
}

/// The local IP that routes to the internet — a connected UDP socket picks it
/// without sending a packet.
async fn default_route_ip() -> Option<std::net::IpAddr> {
    let probe = tokio::net::UdpSocket::bind("0.0.0.0:0").await.ok()?;
    probe.connect("8.8.8.8:53").await.ok()?;
    Some(probe.local_addr().ok()?.ip())
}

async fn detect_public_ip(http: &reqwest::Client) -> Result<String> {
    let ip = http
        .get("https://api.ipify.org")
        // Without this, blocked egress would hang startup forever, before
        // the HTTP listener even binds.
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("detecting public ip via api.ipify.org (set rtc.builtin.public_ip to skip)")?
        .error_for_status()
        .context("api.ipify.org error")?
        .text()
        .await
        .context("reading api.ipify.org response")?
        .trim()
        .to_string();
    tracing::info!("builtin SFU: auto-detected public ip {ip}");
    Ok(ip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use webrtc::api::media_engine::{MIME_TYPE_OPUS, MIME_TYPE_VP8};
    use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
    use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;

    /// A stand-in for a browser: plain peer connection, default codecs.
    async fn client_pc() -> Arc<RTCPeerConnection> {
        let mut media = MediaEngine::default();
        media.register_default_codecs().unwrap();
        let registry = register_default_interceptors(Registry::new(), &mut media).unwrap();
        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .build();
        Arc::new(api.new_peer_connection(RTCConfiguration::default()).await.unwrap())
    }

    /// End-to-end over loopback: a publisher client offers a video track, the
    /// SFU answers and registers it, a viewer client pulls it and must
    /// receive forwarded RTP.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn publish_forward_subscribe() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .try_init();
        let cfg = BuiltinRtcConfig { udp_port: 0, public_ip: Some("127.0.0.1".into()) };
        let sfu = Sfu::new(&cfg, &reqwest::Client::new()).await.unwrap();

        // Publisher leg: client offers, SFU answers.
        let pub_pc = client_pc().await;
        let pub_track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_VP8.to_owned(),
                clock_rate: 90000,
                ..Default::default()
            },
            "video".to_owned(),
            "pub-stream".to_owned(),
        ));
        let pub_audio_track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                ..Default::default()
            },
            "audio".to_owned(),
            "pub-stream".to_owned(),
        ));
        let sendonly = || {
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Sendonly,
                send_encodings: vec![],
            })
        };
        let pub_transceiver = pub_pc
            .add_transceiver_from_track(
                pub_track.clone() as Arc<dyn TrackLocal + Send + Sync>,
                sendonly(),
            )
            .await
            .unwrap();
        let pub_audio_transceiver = pub_pc
            .add_transceiver_from_track(
                pub_audio_track.clone() as Arc<dyn TrackLocal + Send + Sync>,
                sendonly(),
            )
            .await
            .unwrap();
        let offer = pub_pc.create_offer(None).await.unwrap();
        let mut gathered = pub_pc.gathering_complete_promise().await;
        pub_pc.set_local_description(offer).await.unwrap();
        let _ = gathered.recv().await;
        let offer_sdp = pub_pc.local_description().await.unwrap().sdp;
        let mid = pub_transceiver.mid().unwrap().to_string();
        let audio_mid = pub_audio_transceiver.mid().unwrap().to_string();

        let pub_session = sfu.new_session("publisher-user").await.unwrap();
        let answer_sdp = sfu
            .publish(
                &pub_session,
                &offer_sdp,
                &[(mid, "test:v".to_owned()), (audio_mid, "test:a".to_owned())],
            )
            .await
            .unwrap();
        // The stereo munge must have kicked in on the opus fmtp echo.
        assert!(answer_sdp.contains("stereo=1"), "answer lacks stereo munge:\n{answer_sdp}");
        pub_pc
            .set_remote_description(RTCSessionDescription::answer(answer_sdp).unwrap())
            .await
            .unwrap();

        // Keep RTP flowing; the SFU registers tracks on first media.
        let writer = tokio::spawn(async move {
            let mut seq: u16 = 0;
            loop {
                let packet = webrtc::rtp::packet::Packet {
                    header: webrtc::rtp::header::Header {
                        version: 2,
                        sequence_number: seq,
                        timestamp: u32::from(seq) * 3000,
                        ..Default::default()
                    },
                    payload: bytes::Bytes::from(vec![0u8; 100]),
                };
                let _ = pub_track.write_rtp(&packet).await;
                let _ = pub_audio_track.write_rtp(&packet).await;
                seq = seq.wrapping_add(1);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });

        // The publisher leg needs ICE+DTLS before media (and thus the track
        // registry entries) exists.
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let tracks = sfu.tracks.read().await;
                if tracks.contains_key("test:v") && tracks.contains_key("test:a") {
                    break;
                }
                drop(tracks);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("publisher tracks never registered");

        // Viewer leg: SFU offers, client answers.
        let viewer_session = sfu.new_session("viewer-user").await.unwrap();
        let (viewer_offer, tracks) = sfu
            .subscribe(&viewer_session, &["test:v".to_owned(), "test:a".to_owned()])
            .await
            .unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].1, "test:v");
        assert_eq!(tracks[1].1, "test:a");

        let viewer_pc = client_pc().await;
        let (got_rtp_tx, mut got_rtp_rx) = tokio::sync::mpsc::channel::<RTPCodecType>(2);
        viewer_pc.on_track(Box::new(move |track, _receiver, _transceiver| {
            let got_rtp_tx = got_rtp_tx.clone();
            Box::pin(async move {
                if track.read_rtp().await.is_ok() {
                    let _ = got_rtp_tx.send(track.kind()).await;
                }
            })
        }));
        viewer_pc
            .set_remote_description(RTCSessionDescription::offer(viewer_offer).unwrap())
            .await
            .unwrap();
        let viewer_answer = viewer_pc.create_answer(None).await.unwrap();
        let mut gathered = viewer_pc.gathering_complete_promise().await;
        viewer_pc.set_local_description(viewer_answer).await.unwrap();
        let _ = gathered.recv().await;
        let viewer_answer_sdp = viewer_pc.local_description().await.unwrap().sdp;
        sfu.accept_answer(&viewer_session, &viewer_answer_sdp).await.unwrap();

        let (mut got_video, mut got_audio) = (false, false);
        while !(got_video && got_audio) {
            let kind = tokio::time::timeout(Duration::from_secs(15), got_rtp_rx.recv())
                .await
                .expect("viewer never received forwarded RTP on both tracks")
                .unwrap();
            got_video |= kind == RTPCodecType::Video;
            got_audio |= kind == RTPCodecType::Audio;
        }

        // A duplicate pull must be rejected, not silently duplicate media.
        assert!(sfu.subscribe(&viewer_session, &["test:v".to_owned()]).await.is_err());

        // Unsubscribe the video: viewer stops the transceiver, offers, SFU
        // answers and frees the name for a later re-pull.
        let video_mid = tracks[0].0.clone();
        for transceiver in viewer_pc.get_transceivers().await {
            if transceiver.mid().as_deref() == Some(video_mid.as_str()) {
                transceiver.stop().await.unwrap();
            }
        }
        let close_offer = viewer_pc.create_offer(None).await.unwrap();
        let mut gathered = viewer_pc.gathering_complete_promise().await;
        viewer_pc.set_local_description(close_offer).await.unwrap();
        let _ = gathered.recv().await;
        let close_offer_sdp = viewer_pc.local_description().await.unwrap().sdp;
        let close_answer = sfu
            .unsubscribe(&viewer_session, &[video_mid], &close_offer_sdp)
            .await
            .unwrap();
        viewer_pc
            .set_remote_description(RTCSessionDescription::answer(close_answer).unwrap())
            .await
            .unwrap();

        // The same track may now be pulled again on a fresh mid.
        let (_offer, repulled) =
            sfu.subscribe(&viewer_session, &["test:v".to_owned()]).await.unwrap();
        assert_eq!(repulled.len(), 1);
        assert_eq!(repulled[0].1, "test:v");

        writer.abort();
        sfu.close_session(&pub_session).await;
        sfu.close_session(&viewer_session).await;
    }
}
