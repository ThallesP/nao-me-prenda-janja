import type { DiscordSDK } from "@discord/embedded-app-sdk";
import { useCallback, useEffect, useRef, useState } from "react";

import { ScreenShareIcon } from "../components/icons";
import { StreamTile } from "../components/StreamTile";
import { useBestFit } from "../components/useBestFit";
import { createShareLink, getSessionToken } from "../lib/api";
import { setupDiscord } from "../lib/discord";
import { Viewer, type StreamKind } from "../lib/rtc";
import type { PublicUser, Share, ServerMsg } from "../lib/types";
import { connectRoom, type RoomConnection } from "../lib/ws";

type Phase =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "ready" };

type StreamPair = { video?: MediaStream; audio?: MediaStream };

export const Activity = () => {
  const [phase, setPhase] = useState<Phase>({ kind: "loading" });
  const [shares, setShares] = useState<Map<string, Share>>(new Map());
  const [streams, setStreams] = useState<Map<string, StreamPair>>(new Map());
  const [focusedId, setFocusedId] = useState<string | null>(null);
  const [mutedIds, setMutedIds] = useState<Set<string>>(new Set());
  const [unwatchedIds, setUnwatchedIds] = useState<Set<string>>(new Set());
  const [audioBlocked, setAudioBlocked] = useState(false);
  const [audioRetry, setAudioRetry] = useState(0);
  const [barVisible, setBarVisible] = useState(true);
  const [self, setSelf] = useState<PublicUser | null>(null);
  const [loadStep, setLoadStep] = useState("Connecting…");

  const sdkRef = useRef<DiscordSDK | null>(null);
  const viewerRef = useRef<Viewer | null>(null);
  const connIdRef = useRef<number | null>(null);
  const connRef = useRef<RoomConnection | null>(null);
  const sharesRef = useRef<Map<string, Share>>(new Map());
  /**
   * Per share: which kinds are currently subscribed at the SFU, plus a
   * per-kind sequence bumped on every new pull so a stale pull's failure
   * callback can't clobber the state of a newer attempt.
   */
  const appliedRef = useRef<
    Map<string, { video: boolean; audio: boolean; seq: { video: number; audio: number } }>
  >(new Map());
  const unwatchedRef = useRef<Set<string>>(new Set());
  const mutedRef = useRef<Set<string>>(new Set());
  const hiddenRef = useRef(document.hidden);
  const idleTimer = useRef<ReturnType<typeof setTimeout>>(undefined);

  const onTrack = useCallback(
    ({ shareId, kind, stream }: { shareId: string; kind: "video" | "audio"; stream: MediaStream }) => {
      setStreams((prev) => {
        const next = new Map(prev);
        next.set(shareId, { ...next.get(shareId), [kind]: stream });
        return next;
      });
    },
    [],
  );

  /**
   * Tell the server which streams this peer is subscribed to (sent on intent,
   * before pulls land) — it aggregates the counts so publishers can pause
   * encoders nobody is receiving.
   */
  const sendWatching = useCallback(() => {
    const video: string[] = [];
    const audio: string[] = [];
    for (const share of sharesRef.current.values()) {
      if (unwatchedRef.current.has(share.id)) continue;
      // Hidden activity drops video but keeps listening, Discord-style.
      if (!hiddenRef.current) video.push(share.id);
      if (share.has_audio && !mutedRef.current.has(share.id)) audio.push(share.id);
    }
    connRef.current?.send({ type: "watching", video, audio });
  }, []);

  const dropStreams = useCallback((id: string, kinds: StreamKind[]) => {
    setStreams((prev) => {
      const pair = prev.get(id);
      if (!pair) return prev;
      const next = new Map(prev);
      const rest = { ...pair };
      for (const kind of kinds) delete rest[kind];
      if (rest.video || rest.audio) next.set(id, rest);
      else next.delete(id);
      return next;
    });
  }, []);

  /**
   * Diff desired subscriptions (share exists, tile watched, tab visible,
   * audio unmuted) against what's actually pulled, then pull/unpull the
   * difference. Failed pulls retry while the share still exists — otherwise
   * one transient SFU hiccup would leave a tile stuck on "Connecting…".
   */
  const reconcile = useCallback(() => {
    const viewer = viewerRef.current;
    if (!viewer) return;
    const applied = appliedRef.current;
    for (const id of [...applied.keys()]) {
      if (!sharesRef.current.has(id)) applied.delete(id);
    }
    sendWatching();
    for (const share of sharesRef.current.values()) {
      const state = applied.get(share.id) ?? {
        video: false,
        audio: false,
        seq: { video: 0, audio: 0 },
      };
      applied.set(share.id, state);
      const unwatched = unwatchedRef.current.has(share.id);
      const want = {
        video: !unwatched && !hiddenRef.current,
        audio: share.has_audio && !unwatched && !mutedRef.current.has(share.id),
      };
      const add = (["video", "audio"] as const).filter((k) => want[k] && !state[k]);
      const remove = (["video", "audio"] as const).filter((k) => !want[k] && state[k]);
      for (const kind of add) {
        state[kind] = true;
        state.seq[kind] += 1;
      }
      for (const kind of remove) state[kind] = false;
      if (add.length > 0) {
        const seqs = add.map((kind) => state.seq[kind]);
        viewer.watch(share.id, add).then((ok) => {
          if (ok || viewerRef.current !== viewer) return;
          const current = appliedRef.current.get(share.id);
          if (current) {
            // Only roll back kinds no newer pull has touched since.
            add.forEach((kind, i) => {
              if (current.seq[kind] === seqs[i]) current[kind] = false;
            });
          }
          setTimeout(() => {
            if (viewerRef.current === viewer && sharesRef.current.has(share.id)) reconcile();
          }, 3000);
        });
      }
      if (remove.length > 0) {
        dropStreams(share.id, remove);
        viewer.unwatch(share.id, remove);
      }
    }
  }, [sendWatching, dropStreams]);

  const rebuildViewer = useCallback(() => {
    viewerRef.current?.close();
    appliedRef.current = new Map();
    setStreams(new Map());
    const connId = connIdRef.current;
    if (connId === null) return;
    viewerRef.current = new Viewer(connId, onTrack, () => rebuildViewer());
    reconcile();
  }, [onTrack, reconcile]);

  const applyShares = useCallback(
    (next: Map<string, Share>) => {
      sharesRef.current = next;
      setShares(next);
      reconcile();
      setStreams((prev) => {
        const filtered = new Map([...prev].filter(([id]) => next.has(id)));
        return filtered.size === prev.size ? prev : filtered;
      });
      setFocusedId((prev) => (prev && next.has(prev) ? prev : null));
    },
    [reconcile],
  );

  const onMessage = useCallback(
    (msg: ServerMsg) => {
      if (msg.type === "hello") {
        if (connIdRef.current !== msg.conn_id) {
          connIdRef.current = msg.conn_id;
          rebuildViewer();
        }
        applyShares(new Map(msg.shares.map((s) => [s.id, s])));
      } else if (msg.type === "share_started") {
        applyShares(new Map(sharesRef.current).set(msg.share.id, msg.share));
      } else if (msg.type === "share_ended") {
        const next = new Map(sharesRef.current);
        next.delete(msg.share_id);
        applyShares(next);
      }
    },
    [applyShares, rebuildViewer],
  );

  useEffect(() => {
    let cancelled = false;

    setupDiscord(setLoadStep)
      .then(({ sdk, user }) => {
        if (cancelled) return;
        sdkRef.current = sdk;
        setSelf(user);
        const token = getSessionToken();
        if (!token) throw new Error("missing session token");
        connRef.current = connectRoom(token, {
          onMessage,
          onAuthFailed: () => {
            connIdRef.current = null;
            viewerRef.current?.close();
            setPhase({ kind: "error", message: "You're no longer in this activity." });
          },
          onRejected: (reason) => {
            connIdRef.current = null;
            viewerRef.current?.close();
            setPhase({ kind: "error", message: reason });
          },
        });
        setPhase({ kind: "ready" });
      })
      .catch((err: unknown) => {
        console.error(err);
        if (!cancelled) {
          setPhase({ kind: "error", message: "Couldn't connect to Discord. Reopen the activity." });
        }
      });

    return () => {
      cancelled = true;
      connIdRef.current = null;
      connRef.current?.dispose();
      viewerRef.current?.close();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Discord pauses hidden streams to save CPU and bandwidth — same here: a
  // hidden activity drops its video subscriptions (audio keeps playing) and
  // re-pulls them when it becomes visible again.
  useEffect(() => {
    const onVisibility = () => {
      hiddenRef.current = document.hidden;
      reconcile();
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => document.removeEventListener("visibilitychange", onVisibility);
  }, [reconcile]);

  // Discord-style auto-hiding control bar.
  useEffect(() => {
    const poke = () => {
      setBarVisible(true);
      clearTimeout(idleTimer.current);
      idleTimer.current = setTimeout(() => setBarVisible(false), 3000);
    };
    poke();
    window.addEventListener("mousemove", poke);
    return () => {
      window.removeEventListener("mousemove", poke);
      clearTimeout(idleTimer.current);
    };
  }, []);

  const openSharePage = () => {
    const sdk = sdkRef.current;
    if (!sdk) return;
    createShareLink()
      .then(({ url }) => sdk.commands.openExternalLink({ url }))
      .catch((err: unknown) => console.error("share link failed", err));
  };

  // Mute unpulls the audio track (not just element-level mute) so a muted
  // stream costs no audio bandwidth; unmute re-pulls it.
  const toggleMute = (id: string) => {
    const next = new Set(mutedRef.current);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    mutedRef.current = next;
    setMutedIds(next);
    reconcile();
  };

  // Per-stream opt-out: an unwatched tile receives nothing at all.
  const toggleWatch = (id: string) => {
    const next = new Set(unwatchedRef.current);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    unwatchedRef.current = next;
    setUnwatchedIds(next);
    reconcile();
  };

  const unlockAudio = () => {
    setAudioBlocked(false);
    setAudioRetry((n) => n + 1);
  };

  const onAudioBlocked = useCallback(() => setAudioBlocked(true), []);

  const shareList = [...shares.values()];
  const isSelfLive = self !== null && shareList.some((share) => share.user_id === self.id);
  const focused = focusedId ? shares.get(focusedId) : undefined;
  const gridCount = shareList.length;
  const { ref: gridRef, size } = useBestFit(gridCount);

  const tileFor = (share: Share, compact: boolean, style?: React.CSSProperties) => (
    <StreamTile
      key={share.id}
      share={share}
      video={streams.get(share.id)?.video}
      audio={streams.get(share.id)?.audio}
      muted={mutedIds.has(share.id)}
      watching={!unwatchedIds.has(share.id)}
      audioRetry={audioRetry}
      compact={compact}
      style={style}
      onClick={() => setFocusedId(focusedId === share.id ? null : share.id)}
      onToggleMute={() => toggleMute(share.id)}
      onToggleWatch={() => toggleWatch(share.id)}
      onAudioBlocked={onAudioBlocked}
    />
  );

  if (phase.kind === "loading") {
    return (
      <div className="activity-root center-stack">
        <div className="spinner" />
        <div className="hint-text">{loadStep}</div>
      </div>
    );
  }

  if (phase.kind === "error") {
    return (
      <div className="activity-root center-stack">
        <div className="error-title">Something went wrong</div>
        <div className="hint-text">{phase.message}</div>
      </div>
    );
  }

  return (
    <div className="activity-root">
      <div className="stage">
        {gridCount === 0 && (
          <div className="empty-state">
            <div className="empty-icon">
              <ScreenShareIcon size={36} />
            </div>
            <div className="empty-title">No one is streaming</div>
            <div className="empty-sub">
              Share your screen so everyone in the call can watch — it opens in your browser.
            </div>
            <button type="button" className="btn-blurple" onClick={openSharePage}>
              Share Your Screen
            </button>
          </div>
        )}

        {gridCount > 0 && !focused && (
          <div className="grid" ref={gridRef}>
            {shareList.map((share) =>
              tileFor(share, false, { width: size.width, height: size.height }),
            )}
          </div>
        )}

        {focused && (
          <div className="stage-focus">
            <div className="focus-main">{tileFor(focused, false)}</div>
            {shareList.length > 1 && (
              <div className="focus-strip">
                {shareList
                  .filter((share) => share.id !== focused.id)
                  .map((share) => tileFor(share, true, { width: 174, height: 98 }))}
              </div>
            )}
          </div>
        )}

        {audioBlocked && (
          <button type="button" className="audio-toast" onClick={unlockAudio}>
            Click to enable stream audio
          </button>
        )}
      </div>

      <div className={`control-bar${barVisible ? "" : " control-bar-hidden"}`}>
        <div className="control-pill">
          <button
            type="button"
            className={`control-btn${isSelfLive ? " control-btn-active" : ""}`}
            data-tooltip={isSelfLive ? "You're live — manage in the share tab" : "Share Your Screen"}
            onClick={openSharePage}
          >
            <ScreenShareIcon size={24} />
          </button>
        </div>
      </div>
    </div>
  );
};
