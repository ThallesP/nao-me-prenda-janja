/**
 * Inside the activity iframe, Discord's proxy injects a script that sets
 * `RTCPeerConnection = null` (and `WebTransport = null`) on the top window to
 * enforce "no WebRTC". That assignment only touches the top document's global
 * — a nested same-origin `about:blank` iframe gets its own fresh Window whose
 * native WebRTC constructors are intact. We borrow them from there.
 *
 * The helper iframe is kept attached for the page's lifetime: the constructors
 * belong to its realm, so it must stay alive while any PeerConnection exists.
 */
export type RtcRealm = {
  RTCPeerConnection: typeof RTCPeerConnection;
  RTCRtpReceiver: typeof RTCRtpReceiver;
  RTCRtpSender: typeof RTCRtpSender;
  // Tracks come out of this realm's PeerConnection, so wrap them in this
  // realm's MediaStream too — mixing constructors across realms is brittle.
  MediaStream: typeof MediaStream;
};

let cached: RtcRealm | null = null;

const fromWindow = (w: Window & typeof globalThis): RtcRealm | null => {
  if (typeof w.RTCPeerConnection !== "function") return null;
  return {
    RTCPeerConnection: w.RTCPeerConnection,
    RTCRtpReceiver: w.RTCRtpReceiver,
    RTCRtpSender: w.RTCRtpSender,
    MediaStream: w.MediaStream,
  };
};

export const getRtcRealm = (): RtcRealm => {
  if (cached) return cached;

  // Publisher page (external, not proxied) — the real constructors are here.
  const own = fromWindow(window);
  if (own) {
    cached = own;
    return own;
  }

  // Activity iframe — recover from a nested realm.
  const frame = document.createElement("iframe");
  frame.setAttribute("aria-hidden", "true");
  frame.style.display = "none";
  document.body.appendChild(frame);
  const child = frame.contentWindow as (Window & typeof globalThis) | null;
  const recovered = child && fromWindow(child);
  if (!recovered) {
    throw new Error("WebRTC is unavailable and could not be recovered in this environment");
  }
  cached = recovered;
  return recovered;
};
