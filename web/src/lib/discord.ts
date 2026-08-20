import { DiscordSDK } from "@discord/embedded-app-sdk";

import { exchangeToken, fetchPublicConfig, setSessionToken } from "./api";
import { apiBase } from "./env";
import type { PublicUser } from "./types";

declare global {
  interface Window {
    __NMPJ_CLIENT_ID?: string;
  }
}

const eagerClientId = window.__NMPJ_CLIENT_ID;
const inActivityFrame = new URLSearchParams(window.location.search).has("frame_id");

// NOTE: there used to be a "hello bridge" here that re-sent the SDK's
// HANDSHAKE whenever the client posted a HELLO (opcode 3). The hang it worked
// around was actually BetterDiscord swallowing the first handshake, and the
// duplicate HANDSHAKE makes the client close the socket ("Already connected"),
// which left authorize() hanging. HELLO is legacy and safe to ignore.

// Construct the SDK synchronously at module scope, like the official
// examples. The client id is injected into index.html by the server.
const eagerSdk = eagerClientId && inActivityFrame ? new DiscordSDK(eagerClientId) : null;

/** Fire-and-forget diagnostics to the server log when the handshake stalls. */
const reportStuck = (step: string) => {
  const payload = {
    step,
    referrer: document.referrer,
    page: location.origin + location.pathname,
    params: [...new URLSearchParams(location.search).keys()],
    ua: navigator.userAgent,
  };
  fetch(`${apiBase}/api/debug`, { method: "POST", body: JSON.stringify(payload) }).catch(() => {});
};

export type ActivitySession = {
  sdk: DiscordSDK;
  user: PublicUser;
};

/**
 * Full embedded-app handshake: ready → authorize (OAuth code, no visible
 * prompt after first consent) → exchange on our server (which also verifies
 * the user is a participant of this activity instance) → authenticate.
 */
export const setupDiscord = async (
  onStep: (step: string) => void = () => {},
): Promise<ActivitySession> => {
  let sdk = eagerSdk;
  let clientId = eagerClientId ?? "";
  if (!sdk) {
    onStep("Loading configuration…");
    const cfg = await fetchPublicConfig();
    clientId = cfg.client_id;
    sdk = new DiscordSDK(clientId);
  }

  onStep("Waiting for Discord…");
  const readyBeacon = setTimeout(() => reportStuck("ready"), 6000);
  await sdk.ready();
  clearTimeout(readyBeacon);

  onStep("Waiting for authorization…");
  const authBeacon = setTimeout(() => reportStuck("authorize"), 30_000);
  const { code } = await sdk.commands.authorize({
    client_id: clientId,
    response_type: "code",
    state: "",
    prompt: "none",
    scope: ["identify"],
  });
  clearTimeout(authBeacon);

  onStep("Signing in…");
  const auth = await exchangeToken({ code, instance_id: sdk.instanceId });
  setSessionToken(auth.session_token);
  await sdk.commands.authenticate({ access_token: auth.access_token });
  onStep("Joining…");
  return { sdk, user: auth.user };
};
