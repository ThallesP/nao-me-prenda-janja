import { useEffect, useRef, type CSSProperties } from "react";

import type { Share } from "../lib/types";
import { avatarUrl } from "../lib/types";
import { EyeIcon, EyeOffIcon, FullscreenIcon, SoundIcon, SoundMutedIcon } from "./icons";

type Props = {
  share: Share;
  video?: MediaStream;
  audio?: MediaStream;
  muted: boolean;
  /** False when the user opted out of this stream — nothing is received. */
  watching: boolean;
  /** Bumped after a user gesture to retry audio playback blocked by autoplay. */
  audioRetry: number;
  compact?: boolean;
  style?: CSSProperties;
  onClick: () => void;
  onToggleMute: () => void;
  onToggleWatch: () => void;
  onAudioBlocked: () => void;
};

export const StreamTile = ({
  share,
  video,
  audio,
  muted,
  watching,
  audioRetry,
  compact,
  style,
  onClick,
  onToggleMute,
  onToggleWatch,
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
      {video && watching ? (
        <video ref={videoRef} muted playsInline />
      ) : (
        <div className="tile-placeholder">
          <img src={avatarUrl(share.user_id, share.avatar)} alt="" draggable={false} />
          {watching ? (
            <div className="tile-connecting">Connecting to stream…</div>
          ) : (
            <button
              type="button"
              className="btn-blurple"
              onClick={(e) => {
                e.stopPropagation();
                onToggleWatch();
              }}
            >
              <EyeIcon size={18} />
              Watch Stream
            </button>
          )}
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
              {watching && share.has_audio && (
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
                data-tooltip={watching ? "Stop Watching" : "Watch Stream"}
                onClick={onToggleWatch}
              >
                {watching ? <EyeOffIcon size={18} /> : <EyeIcon size={18} />}
              </button>
              {watching && (
                <button
                  type="button"
                  className="tile-btn"
                  data-tooltip="Full Screen"
                  onClick={goFullscreen}
                >
                  <FullscreenIcon size={18} />
                </button>
              )}
            </div>
          )}
        </div>
      </div>
    </div>
  );
};
