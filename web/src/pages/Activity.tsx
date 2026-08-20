import type { DiscordSDK } from "@discord/embedded-app-sdk";
import { useCallback, useEffect, useRef, useState } from "react";

import { ScreenShareIcon } from "../components/icons";
import { StreamTile } from "../components/StreamTile";
import { useBestFit } from "../components/useBestFit";
import { createShareLink, getSessionToken } from "../lib/api";
import { setupDiscord } from "../lib/discord";
import { Viewer } from "../lib/rtc";
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
  const [audioBlocked, setAudioBlocked] = useState(false);
  const [audioRetry, setAudioRetry] = useState(0);
  const [barVisible, setBarVisible] = useState(true);
  const [self, setSelf] = useState<PublicUser | null>(null);
  const [loadStep, setLoadStep] = useState("Connecting…");

  const sdkRef = useRef<DiscordSDK | null>(null);
  const viewerRef = useRef<Viewer | null>(null);
  const watchedRef = useRef<Set<string>>(new Set());
  const sharesRef = useRef<Map<string, Share>>(new Map());
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
   * Watch a share, un-marking it and retrying while it still exists if the
   * pull fails — otherwise one transient SFU hiccup would leave the tile
   * stuck on "Connecting…" forever.
   */
  const watchShare = useCallback((id: string) => {
    const viewer = viewerRef.current;
    if (!viewer || watchedRef.current.has(id)) return;
    watchedRef.current.add(id);
    viewer.watch(id).then((ok) => {
      if (ok || viewerRef.current !== viewer) return;
      watchedRef.current.delete(id);
      setTimeout(() => {
        if (viewerRef.current === viewer && sharesRef.current.has(id)) {
          watchShare(id);
        }
      }, 3000);
    });
  }, []);

  const rebuildViewer = useCallback(() => {
    viewerRef.current?.close();
    watchedRef.current = new Set();
    viewerRef.current = new Viewer(onTrack, () => rebuildViewer());
    setStreams(new Map());
    for (const id of sharesRef.current.keys()) {
      watchShare(id);
    }
  }, [onTrack, watchShare]);

  const applyShares = useCallback(
    (next: Map<string, Share>) => {
      sharesRef.current = next;
      setShares(next);
      for (const id of next.keys()) {
        watchShare(id);
      }
      setStreams((prev) => {
        const filtered = new Map([...prev].filter(([id]) => next.has(id)));
        return filtered.size === prev.size ? prev : filtered;
      });
      setFocusedId((prev) => (prev && next.has(prev) ? prev : null));
    },
    [watchShare],
  );

  const onMessage = useCallback(
    (msg: ServerMsg) => {
      if (msg.type === "hello") {
        applyShares(new Map(msg.shares.map((s) => [s.id, s])));
      } else if (msg.type === "share_started") {
        applyShares(new Map(sharesRef.current).set(msg.share.id, msg.share));
      } else if (msg.type === "share_ended") {
        const next = new Map(sharesRef.current);
        next.delete(msg.share_id);
        watchedRef.current.delete(msg.share_id);
        applyShares(next);
      }
    },
    [applyShares],
  );

  useEffect(() => {
    let conn: RoomConnection | undefined;
    let cancelled = false;
    viewerRef.current = new Viewer(onTrack, () => rebuildViewer());

    setupDiscord(setLoadStep)
      .then(({ sdk, user }) => {
        if (cancelled) return;
        sdkRef.current = sdk;
        setSelf(user);
        const token = getSessionToken();
        if (!token) throw new Error("missing session token");
        conn = connectRoom(token, {
          onMessage,
          onAuthFailed: () =>
            setPhase({ kind: "error", message: "You're no longer in this activity." }),
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
      conn?.dispose();
      viewerRef.current?.close();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

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

  const toggleMute = (id: string) => {
    setMutedIds((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
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
      audioRetry={audioRetry}
      compact={compact}
      style={style}
      onClick={() => setFocusedId(focusedId === share.id ? null : share.id)}
      onToggleMute={() => toggleMute(share.id)}
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
