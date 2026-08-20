// Inside the activity iframe we're on <app-id>.discordsays.com and every
// network request must go through Discord's proxy path prefix. On our own
// domain (the /share page) we talk to the server directly.
export const isEmbedded = location.hostname.endsWith(".discordsays.com");

export const apiBase = isEmbedded ? "/.proxy" : "";

export const wsUrl = () => {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  return `${proto}://${location.host}${apiBase}/api/ws`;
};
