import { useEffect, useRef, useState } from "react";

import { ScreenShareIcon, StopIcon } from "../components/icons";
import { claimShareCode, setSessionToken } from "../lib/api";
import { publishScreen, type Publisher } from "../lib/rtc";
import type { PublicUser, ServerMsg } from "../lib/types";
import { avatarUrl } from "../lib/types";
import { connectRoom, type RoomConnection } from "../lib/ws";

type Phase =
  | { kind: "claiming" }
  | { kind: "invalid"; message: string }
  | { kind: "ready" }
  | { kind: "live" }
  | { kind: "ended" }
  | { kind: "error"; message: string };

/**
 * Resolution presets. `width: null` means "don't constrain" — capture the
 * source at its native resolution (up to 4K). Constraints only ever DOWNSCALE
 * the captured surface, never upscale, so picking 1080p on a 4K monitor gives
 * 1080p while picking 4K on a 1080p monitor stays 1080p. Bitrate is matched to
 * the resolution so a low pick doesn't waste bandwidth.
 */
type ResolutionPreset = {
  id: string;
  label: string;
  width: number | null;
  height: number | null;
  bitrate: number;
};

const RESOLUTIONS: ResolutionPreset[] = [
  { id: "source", label: "Source (up to 4K)", width: null, height: null, bitrate: 20_000_000 },
  { id: "2160", label: "2160p — 4K", width: 3840, height: 2160, bitrate: 20_000_000 },
  { id: "1440", label: "1440p — 2K", width: 2560, height: 1440, bitrate: 12_000_000 },
  { id: "1080", label: "1080p — Full HD", width: 1920, height: 1080, bitrate: 8_000_000 },
  { id: "720", label: "720p — HD", width: 1280, height: 720, bitrate: 4_000_000 },
];

const FRAME_RATES = [
  { id: "60", label: "60 fps", value: 60 },
  { id: "30", label: "30 fps", value: 30 },
];

/**
 * Build capture constraints for a preset. Note constraints never filter the
 * picker — they only scale the result. Monitor capture delivers physical
 * pixels (tabs capture at CSS pixels).
 */
const buildConstraints = (res: ResolutionPreset, fps: number): MediaStreamConstraints =>
  ({
    video: {
      ...(res.width && res.height
        ? { width: { ideal: res.width }, height: { ideal: res.height } }
        : {}),
      frameRate: { ideal: fps, max: fps },
    },
    audio: {
      // Content audio: voice processing would mangle music/video sound.
      echoCancellation: false,
      noiseSuppression: false,
      autoGainControl: false,
      suppressLocalAudioPlayback: true,
      channelCount: 2,
      sampleRate: 48000,
    },
    systemAudio: "include",
    surfaceSwitching: "include",
    selfBrowserSurface: "exclude",
    monitorTypeSurfaces: "include",
  }) as MediaStreamConstraints;

export const Share = () => {
  const [phase, setPhase] = useState<Phase>({ kind: "claiming" });
  const [user, setUser] = useState<PublicUser | null>(null);
  const [resolutionId, setResolutionId] = useState("source");
  const [fpsId, setFpsId] = useState("60");
  const connIdRef = useRef<number | null>(null);
  const connRef = useRef<RoomConnection | null>(null);
  const publisherRef = useRef<Publisher | null>(null);
  const streamRef = useRef<MediaStream | null>(null);
  const startingRef = useRef(false);
  const previewRef = useRef<HTMLVideoElement>(null);

  const endLocal = () => {
    publisherRef.current?.close();
    publisherRef.current = null;
    streamRef.current = null;
    setPhase((prev) => (prev.kind === "live" ? { kind: "ended" } : prev));
  };

  const stopStreaming = () => {
    const id = publisherRef.current?.shareId;
    // The websocket is the reliable stop path: if it's down, the server
    // already tore the share down when the connection dropped.
    if (id) connRef.current?.send({ type: "stop_share", share_id: id });
    endLocal();
  };

  useEffect(() => {
    const code = new URLSearchParams(location.search).get("code");
    if (!code) {
      setPhase({ kind: "invalid", message: "Missing share code. Open this page from the activity." });
      return;
    }
    let cancelled = false;

    claimShareCode(code)
      .then((res) => {
        if (cancelled) return;
        setSessionToken(res.session_token);
        setUser(res.user);
        connRef.current = connectRoom(res.session_token, {
          onMessage: (msg: ServerMsg) => {
            if (msg.type === "hello") {
              connIdRef.current = msg.conn_id;
              // A reconnect means the server already ended our share when the
              // old connection died — don't keep pretending we're live.
              if (publisherRef.current) {
                endLocal();
                return;
              }
              setPhase((prev) => (prev.kind === "claiming" ? { kind: "ready" } : prev));
            }
            if (msg.type === "share_ended" && msg.share_id === publisherRef.current?.shareId) {
              endLocal();
            }
          },
          onAuthFailed: () =>
            setPhase({ kind: "invalid", message: "You're no longer in the activity's voice channel." }),
        });
      })
      .catch(() => {
        if (!cancelled) {
          setPhase({
            kind: "invalid",
            message: "This link expired or was already used. Go back to Discord and click Share Your Screen again.",
          });
        }
      });

    const onPageHide = () => publisherRef.current?.close();
    window.addEventListener("pagehide", onPageHide);
    return () => {
      cancelled = true;
      connRef.current?.dispose();
      publisherRef.current?.close();
      window.removeEventListener("pagehide", onPageHide);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const el = previewRef.current;
    const stream = streamRef.current;
    if (phase.kind === "live" && el && stream) {
      el.srcObject = stream;
      el.play().catch(() => {});
    }
  }, [phase]);

  const start = () => {
    const connId = connIdRef.current;
    if (connId === null || startingRef.current || publisherRef.current) return;
    startingRef.current = true;
    const res = RESOLUTIONS.find((r) => r.id === resolutionId) ?? RESOLUTIONS[0];
    const fps = FRAME_RATES.find((f) => f.id === fpsId)?.value ?? 60;
    navigator.mediaDevices
      .getDisplayMedia(buildConstraints(res, fps))
      .then((stream) => {
        streamRef.current = stream;
        const [videoTrack] = stream.getVideoTracks();
        // Browser's own "Stop sharing" bar ends the track — mirror it.
        videoTrack.addEventListener("ended", () => stopStreaming());
        return publishScreen(stream, connId, res.bitrate).then((publisher) => {
          publisherRef.current = publisher;
          setPhase({ kind: "live" });
          // Capture may have been stopped while signaling was in flight.
          if (videoTrack.readyState === "ended") stopStreaming();
        });
      })
      .catch((err: unknown) => {
        // User cancelled the picker — stay put quietly.
        if (err instanceof DOMException && err.name === "NotAllowedError") return;
        console.error(err);
        streamRef.current?.getTracks().forEach((track) => track.stop());
        streamRef.current = null;
        setPhase({ kind: "error", message: "Couldn't start the stream. Try again." });
      })
      .finally(() => {
        startingRef.current = false;
      });
  };

  return (
    <div className="share-root">
      <div className="share-card">
        {phase.kind === "claiming" && (
          <div className="center-stack">
            <div className="spinner" />
            <div className="hint-text">Checking your session…</div>
          </div>
        )}

        {phase.kind === "invalid" && (
          <div className="center-stack">
            <div className="error-title">Can't share</div>
            <div className="hint-text">{phase.message}</div>
          </div>
        )}

        {phase.kind === "error" && (
          <div className="center-stack">
            <div className="error-title">Stream failed</div>
            <div className="hint-text">{phase.message}</div>
            <button type="button" className="btn-blurple" onClick={() => setPhase({ kind: "ready" })}>
              Back
            </button>
          </div>
        )}

        {phase.kind === "ready" && (
          <>
            <div className="share-header">
              {user && <img className="share-avatar" src={avatarUrl(user.id, user.avatar)} alt="" />}
              <div>
                <div className="share-title">Share Your Screen</div>
                <div className="share-sub">
                  Pick a screen or window — everyone in the activity will see it instantly.
                  Keep this tab open while you stream.
                </div>
              </div>
            </div>
            <div className="share-fields">
              <label className="share-field">
                <span className="share-field-label">Resolution</span>
                <select
                  className="share-select"
                  value={resolutionId}
                  onChange={(e) => setResolutionId(e.target.value)}
                >
                  {RESOLUTIONS.map((r) => (
                    <option key={r.id} value={r.id}>
                      {r.label}
                    </option>
                  ))}
                </select>
              </label>
              <label className="share-field">
                <span className="share-field-label">Frame rate</span>
                <select
                  className="share-select"
                  value={fpsId}
                  onChange={(e) => setFpsId(e.target.value)}
                >
                  {FRAME_RATES.map((f) => (
                    <option key={f.id} value={f.id}>
                      {f.label}
                    </option>
                  ))}
                </select>
              </label>
            </div>
            <button type="button" className="btn-blurple btn-large" onClick={start}>
              <ScreenShareIcon size={20} />
              Select window or screen
            </button>
            <div className="share-footnote">
              Higher settings need more upload bandwidth. Check “Share audio” in the picker to
              stream sound too.
            </div>
          </>
        )}

        {phase.kind === "live" && (
          <>
            <div className="share-live-row">
              <span className="live-badge">Live</span>
              <span className="share-title">You're streaming</span>
            </div>
            <video ref={previewRef} className="share-preview" muted playsInline />
            <button type="button" className="btn-danger btn-large" onClick={stopStreaming}>
              <StopIcon size={20} />
              Stop Streaming
            </button>
          </>
        )}

        {phase.kind === "ended" && (
          <div className="center-stack">
            <div className="share-title">Stream ended</div>
            <div className="hint-text">You can close this tab or go live again.</div>
            <button type="button" className="btn-blurple" onClick={start}>
              Share again
            </button>
          </div>
        )}
      </div>
    </div>
  );
};
