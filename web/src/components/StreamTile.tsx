import { useEffect, useRef, type CSSProperties } from "react";

import type { Share } from "../lib/types";
import { avatarUrl } from "../lib/types";
import { FullscreenIcon, SoundIcon, SoundMutedIcon } from "./icons";

type Props = {
  share: Share;
  video?: MediaStream;
  audio?: MediaStream;
  muted: boolean;
  /** Bumped after a user gesture to retry audio playback blocked by autoplay. */
  audioRetry: number;
  compact?: boolean;
  style?: CSSProperties;
  onClick: () => void;
  onToggleMute: () => void;
  onAudioBlocked: () => void;
};

export const StreamTile = ({
  share,
  video,
  audio,
  muted,
  audioRetry,
  compact,
  style,
  onClick,
  onToggleMute,
  onAudioBlocked,
}: Props) => {
  const videoRef = useRef<HTMLVideoElement>(null);
  const audioRef = useRef<HTMLAudioElement>(null);

  useEffect(() => {
    const el = videoRef.current;
    if (!el || !video) return;
    el.srcObject = video;
    el.play().catch(() => {
      /* muted video autoplay is allowed; a race on unmount can still reject */
    });
  }, [video]);

  useEffect(() => {
    const el = audioRef.current;
    if (!el || !audio) return;
    el.srcObject = audio;
    el.muted = muted;
    if (!muted) {
      el.play().catch(() => onAudioBlocked());
    }
  }, [audio, muted, audioRetry, onAudioBlocked]);

  const goFullscreen = () => {
    videoRef.current?.requestFullscreen().catch(() => {});
  };

  return (
    <div
      className={`tile${compact ? " tile-compact" : ""}`}
      style={style}
      onClick={onClick}
      onDoubleClick={goFullscreen}
    >
      {video ? (
        <video ref={videoRef} muted playsInline />
      ) : (
        <div className="tile-placeholder">
          <img src={avatarUrl(share.user_id, share.avatar)} alt="" draggable={false} />
          <div className="tile-connecting">Connecting to stream…</div>
        </div>
      )}
      {share.has_audio && audio && <audio ref={audioRef} autoPlay />}

      <span className="live-badge">Live</span>

      <div className="tile-overlay">
        <div className="tile-controls-row" onClick={(e) => e.stopPropagation()}>
          <div className="tile-pill" onClick={onClick}>
            <span className="tile-name">{share.username}</span>
            {muted && share.has_audio && (
              <span className="tile-muted-icon">
                <SoundMutedIcon size={14} />
              </span>
            )}
          </div>
          {!compact && (
            <div className="tile-actions">
              {share.has_audio && (
                <button
                  type="button"
                  className="tile-btn"
                  data-tooltip={muted ? "Unmute" : "Mute"}
                  onClick={onToggleMute}
                >
                  {muted ? <SoundMutedIcon size={18} /> : <SoundIcon size={18} />}
                </button>
              )}
              <button
                type="button"
                className="tile-btn"
                data-tooltip="Full Screen"
                onClick={goFullscreen}
              >
                <FullscreenIcon size={18} />
              </button>
            </div>
          )}
        </div>
      </div>
    </div>
  );
};
