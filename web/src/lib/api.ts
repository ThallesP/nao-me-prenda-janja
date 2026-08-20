import { apiBase } from "./env";
import type { PublicUser } from "./types";

let sessionToken: string | null = null;

export const setSessionToken = (token: string) => {
  sessionToken = token;
};

export const getSessionToken = () => sessionToken;

const request = async <T>(method: string, path: string, body?: unknown): Promise<T> => {
  const res = await fetch(`${apiBase}${path}`, {
    method,
    headers: {
      "content-type": "application/json",
      ...(sessionToken ? { authorization: `Bearer ${sessionToken}` } : {}),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) {
    const detail = await res
      .json()
      .then((data: { error?: string }) => data.error ?? "")
      .catch(() => "");
    throw new Error(`${method} ${path} → ${res.status}${detail ? `: ${detail}` : ""}`);
  }
  return res.json() as Promise<T>;
};

export const fetchPublicConfig = () =>
  request<{ client_id: string }>("GET", "/api/config-public");

export const exchangeToken = (body: { code: string; instance_id: string }) =>
  request<{ session_token: string; access_token: string; user: PublicUser }>(
    "POST",
    "/api/auth/token",
    body,
  );

export const createShareLink = () =>
  request<{ url: string }>("POST", "/api/share-link");

export const claimShareCode = (code: string) =>
  request<{ session_token: string; user: PublicUser }>("POST", "/api/share/claim", { code });

export const stopShare = (shareId: string) =>
  request<{ ok: boolean }>("POST", "/api/share/stop", { share_id: shareId });

export const rtcNewSession = (connId: number) =>
  request<{ session_id: string }>("POST", "/api/rtc/session", { conn_id: connId });

export const rtcCloseSession = (sessionId: string) =>
  request<{ ok: boolean }>("DELETE", `/api/rtc/session/${sessionId}`);

export const rtcPublish = (
  sessionId: string,
  body: { sdp: string; video_mid: string; audio_mid: string | null; conn_id: number },
) =>
  request<{ share_id: string; answer_sdp: string }>(
    "POST",
    `/api/rtc/session/${sessionId}/publish`,
    body,
  );

export const rtcPull = (sessionId: string, shareId: string, kinds: ("video" | "audio")[]) =>
  request<{
    offer_sdp: string;
    tracks: { mid: string; track_name: string }[];
    requires_renegotiation: boolean;
  }>("POST", `/api/rtc/session/${sessionId}/pull`, { share_id: shareId, kinds });

export const rtcUnpull = (sessionId: string, mids: string[], sdp: string) =>
  request<{ answer_sdp: string }>("POST", `/api/rtc/session/${sessionId}/unpull`, { mids, sdp });

export const rtcRenegotiate = (sessionId: string, sdp: string) =>
  request<{ ok: boolean }>("PUT", `/api/rtc/session/${sessionId}/renegotiate`, { sdp });
