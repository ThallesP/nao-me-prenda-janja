import {
  rtcCloseSession,
  rtcNewSession,
  rtcPublish,
  rtcPull,
  rtcRenegotiate,
  rtcUnpull,
  stopShare,
} from "./api";
import { getRtcRealm } from "./rtcRealm";

const ICE_SERVERS = [{ urls: "stun:stun.cloudflare.com:3478" }];

const newPeerConnection = () => {
  const { RTCPeerConnection } = getRtcRealm();
  return new RTCPeerConnection({ iceServers: ICE_SERVERS, bundlePolicy: "max-bundle" });
};

/**
 * Reorder (never remove) codecs so the SFU picks the best one for screen
 * content. AV1's screen-content tools make 4K text nearly free; VP9 next.
 * The SFU forwards packets untranscoded, so whatever is negotiated here is
 * exactly what viewers receive.
 */
const preferScreenCodecs = (transceiver: RTCRtpTransceiver) => {
  const caps = getRtcRealm().RTCRtpReceiver.getCapabilities("video");
  if (!caps || !transceiver.setCodecPreferences) return;
  const order = ["video/AV1", "video/VP9", "video/H264", "video/VP8"];
  const rank = (mime: string) => {
    const index = order.indexOf(mime);
    return index === -1 ? order.length : index;
  };
  const sorted = [...caps.codecs].sort((a, b) => rank(a.mimeType) - rank(b.mimeType));
  transceiver.setCodecPreferences(sorted);
};

/**
 * Tune the opus params the SFU's answer asks us to send. The encoder obeys
 * the remote description's fmtp, so munging the answer locally is the
 * supported knob: stereo (screen audio is content, not voice), DTX (near-zero
 * bitrate while the shared screen is silent), and a music-friendly ceiling.
 */
const tuneOpusAnswer = (sdp: string): string => {
  const pt = sdp.match(/a=rtpmap:(\d+) opus\/48000\/2/i)?.[1];
  if (!pt) return sdp;
  const fmtpRe = new RegExp(`a=fmtp:${pt} ([^\r\n]*)`);
  const fmtp = sdp.match(fmtpRe);
  if (!fmtp) return sdp;
  let params = fmtp[1];
  for (const [key, value] of [
    ["stereo", "1"],
    ["sprop-stereo", "1"],
    ["usedtx", "1"],
    ["maxaveragebitrate", "128000"],
  ]) {
    if (!new RegExp(`(^|;)${key}=`).test(params)) params += `;${key}=${value}`;
  }
  return sdp.replace(fmtpRe, `a=fmtp:${pt} ${params}`);
};

const waitForConnection = (pc: RTCPeerConnection) =>
  new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("webrtc connection timed out")), 10_000);
    const check = () => {
      const state = pc.iceConnectionState;
      if (state === "connected" || state === "completed") {
        clearTimeout(timer);
        pc.removeEventListener("iceconnectionstatechange", check);
        resolve();
      } else if (state === "failed") {
        clearTimeout(timer);
        pc.removeEventListener("iceconnectionstatechange", check);
        reject(new Error("webrtc connection failed"));
      }
    };
    pc.addEventListener("iceconnectionstatechange", check);
    check();
  });

export type Publisher = {
  pc: RTCPeerConnection;
  shareId: string;
  close: () => void;
};

/**
 * Publish a captured screen (video + optional audio) to the SFU through our
 * backend. 4K-friendly: content hint + high bitrate cap + maintain-resolution.
 */
export const publishScreen = async (
  stream: MediaStream,
  connId: number,
  maxBitrate = 20_000_000,
): Promise<Publisher> => {
  const [videoTrack] = stream.getVideoTracks();
  if (!videoTrack) throw new Error("no video track in capture");
  const { session_id } = await rtcNewSession(connId);
  let pc: RTCPeerConnection;
  try {
    pc = newPeerConnection();
  } catch (err) {
    rtcCloseSession(session_id).catch(() => {});
    stream.getTracks().forEach((track) => track.stop());
    throw err;
  }

  try {
    const [audioTrack] = stream.getAudioTracks();
    // 'detail' keeps text crisp: the encoder holds resolution and drops
    // framerate under pressure instead of blurring the screen.
    videoTrack.contentHint = "detail";

    const videoTx = pc.addTransceiver(videoTrack, { direction: "sendonly" });
    preferScreenCodecs(videoTx);
    const audioTx = audioTrack ? pc.addTransceiver(audioTrack, { direction: "sendonly" }) : null;

    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    if (!videoTx.mid) throw new Error("video mid missing after setLocalDescription");

    const { share_id, answer_sdp } = await rtcPublish(session_id, {
      sdp: offer.sdp ?? "",
      video_mid: videoTx.mid,
      audio_mid: audioTx?.mid ?? null,
      conn_id: connId,
    });

    // The server already announced the share; if anything past this point
    // fails, retract it instead of leaving a dead "Live" tile behind.
    const finalize = async () => {
      await pc.setRemoteDescription({ type: "answer", sdp: tuneOpusAnswer(answer_sdp) });
      const params = videoTx.sender.getParameters();
      params.degradationPreference = "maintain-resolution";
      const encodings = params.encodings.length > 0 ? params.encodings : [{}];
      encodings[0].maxBitrate = maxBitrate; // scaled to the chosen resolution
      encodings[0].priority = "high";
      params.encodings = encodings;
      await videoTx.sender.setParameters(params);
      await waitForConnection(pc);
    };

    return await finalize().then(
      () => ({
        pc,
        shareId: share_id,
        close: () => {
          pc.close();
          stream.getTracks().forEach((track) => track.stop());
          rtcCloseSession(session_id).catch(() => {});
        },
      }),
      (err: unknown) => {
        stopShare(share_id).catch(() => {});
        pc.close();
        stream.getTracks().forEach((track) => track.stop());
        throw err;
      },
    );
  } catch (err) {
    rtcCloseSession(session_id).catch(() => {});
    pc.close();
    stream.getTracks().forEach((track) => track.stop());
    throw err;
  }
};

export type StreamKind = "video" | "audio";

export type ViewerTrack = {
  shareId: string;
  kind: StreamKind;
  stream: MediaStream;
};

/**
 * One receive-only peer connection per viewer; every watched stream's tracks
 * are pulled into it. Pulls are serialized because each one renegotiates the
 * connection.
 */
export class Viewer {
  private pc: RTCPeerConnection | null = null;
  private sessionId: string | null = null;
  private queue: Promise<unknown> = Promise.resolve();
  private closed = false;
  /** mid -> share/kind, filled in as pulls complete. */
  private midMap = new Map<string, { shareId: string; kind: StreamKind }>();
  /** Tracks that arrived before their pull response mapped the mid. */
  private pendingTracks = new Map<string, MediaStreamTrack>();

  constructor(
    private connId: number,
    private onTrack: (track: ViewerTrack) => void,
    private onFailed: () => void,
  ) {}

  /** Resolves false on failure so the caller can retry; never rejects. */
  watch(shareId: string, kinds: StreamKind[]): Promise<boolean> {
    const attempt = this.queue
      .then(() => this.pull(shareId, kinds))
      .then(
        () => true,
        (err: unknown) => {
          if (!this.closed) console.error("pull failed", shareId, err);
          return false;
        },
      );
    this.queue = attempt;
    return attempt;
  }

  /**
   * Stop receiving some of a share's tracks — the SFU stops sending them, so
   * hidden or unwatched streams cost no download bandwidth. Never rejects.
   */
  unwatch(shareId: string, kinds: StreamKind[]): Promise<void> {
    const attempt = this.queue
      .then(() => this.unpull(shareId, kinds))
      .catch((err: unknown) => {
        if (this.closed) return;
        console.error("unpull failed", shareId, err);
        // A failed close can strand the connection mid-negotiation; a
        // rebuild is cheaper than reasoning about every partial state.
        if (this.pc && this.pc.signalingState !== "stable") this.onFailed();
      });
    this.queue = attempt;
    return attempt;
  }

  private async ensurePc(): Promise<{ pc: RTCPeerConnection; sessionId: string }> {
    if (this.pc && this.sessionId) return { pc: this.pc, sessionId: this.sessionId };
    const { session_id } = await rtcNewSession(this.connId);
    if (this.closed) {
      rtcCloseSession(session_id).catch(() => {});
      throw new Error("viewer closed");
    }
    const pc = newPeerConnection();
    pc.ontrack = (event) => {
      const mid = event.transceiver.mid;
      if (!mid) return;
      const mapped = this.midMap.get(mid);
      if (mapped) this.emit(mid, event.track);
      else this.pendingTracks.set(mid, event.track);
    };
    pc.onconnectionstatechange = () => {
      if (pc.connectionState === "failed" && !this.closed) this.onFailed();
    };
    this.pc = pc;
    this.sessionId = session_id;
    return { pc, sessionId: session_id };
  }

  private emit(mid: string, track: MediaStreamTrack) {
    const mapped = this.midMap.get(mid);
    if (!mapped || this.closed) return;
    const stream = new (getRtcRealm().MediaStream)([track]);
    this.onTrack({ shareId: mapped.shareId, kind: mapped.kind, stream });
  }

  private async pull(shareId: string, kinds: StreamKind[]) {
    if (this.closed) throw new Error("viewer closed");
    const { pc, sessionId } = await this.ensurePc();
    const { offer_sdp, tracks, requires_renegotiation } = await rtcPull(sessionId, shareId, kinds);
    for (const { mid, track_name } of tracks) {
      const kind = track_name.endsWith(":a") ? "audio" : "video";
      this.midMap.set(mid, { shareId, kind });
    }
    await pc.setRemoteDescription({ type: "offer", sdp: offer_sdp });
    const answer = await pc.createAnswer();
    await pc.setLocalDescription(answer);
    if (requires_renegotiation) {
      await rtcRenegotiate(sessionId, answer.sdp ?? "");
    }
    // Flush tracks that fired before we knew their mids.
    for (const [mid, track] of [...this.pendingTracks]) {
      if (this.midMap.has(mid)) {
        this.pendingTracks.delete(mid);
        this.emit(mid, track);
      }
    }
  }

  private async unpull(shareId: string, kinds: StreamKind[]) {
    const pc = this.pc;
    const sessionId = this.sessionId;
    if (!pc || !sessionId || this.closed) return;
    const mids = [...this.midMap]
      .filter(([, m]) => m.shareId === shareId && kinds.includes(m.kind))
      .map(([mid]) => mid);
    if (mids.length === 0) return;
    // Stopping the transceiver halts rendering immediately; the offer/answer
    // round tells the SFU to stop sending and frees the m-lines for reuse.
    for (const transceiver of pc.getTransceivers()) {
      if (transceiver.mid && mids.includes(transceiver.mid)) transceiver.stop();
    }
    for (const mid of mids) {
      this.midMap.delete(mid);
      this.pendingTracks.delete(mid);
    }
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    const { answer_sdp } = await rtcUnpull(sessionId, mids, offer.sdp ?? "");
    if (this.closed || this.pc !== pc) return;
    await pc.setRemoteDescription({ type: "answer", sdp: answer_sdp });
  }

  close() {
    this.closed = true;
    const sessionId = this.sessionId;
    this.pc?.close();
    this.pc = null;
    this.sessionId = null;
    this.midMap.clear();
    this.pendingTracks.clear();
    if (sessionId) rtcCloseSession(sessionId).catch(() => {});
  }
}
