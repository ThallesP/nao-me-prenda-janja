import { wsUrl } from "./env";
import type { ServerMsg } from "./types";

export type RoomHandlers = {
  onMessage: (msg: ServerMsg) => void;
  /** Fired when the server rejects the token — no reconnect after this. */
  onAuthFailed: () => void;
  /** Fired when a bounded server connection limit rejects this socket. */
  onRejected: (reason: string) => void;
};

export type RoomConnection = {
  /** Send a client message (dropped silently if the socket isn't open). */
  send: (msg: unknown) => void;
  dispose: () => void;
};

/**
 * Connect to the room websocket with auto-reconnect. The server re-sends a
 * full `hello` after every reconnect, so consumers just reconcile against it.
 */
export const connectRoom = (token: string, handlers: RoomHandlers): RoomConnection => {
  let socket: WebSocket | null = null;
  let disposed = false;
  let attempts = 0;

  const open = () => {
    if (disposed) return;
    socket = new WebSocket(wsUrl());
    socket.onopen = () => {
      socket?.send(JSON.stringify({ token }));
    };
    socket.onmessage = (event) => {
      const msg = JSON.parse(event.data as string) as ServerMsg;
      if (msg.type === "auth_failed") {
        disposed = true;
        handlers.onAuthFailed();
        return;
      }
      if (msg.type === "connection_rejected") {
        disposed = true;
        handlers.onRejected(msg.reason);
        return;
      }
      if (msg.type === "hello") attempts = 0;
      handlers.onMessage(msg);
    };
    socket.onclose = () => {
      if (disposed) return;
      attempts += 1;
      const delay = Math.min(1000 * 2 ** attempts, 10_000);
      setTimeout(open, delay);
    };
  };

  open();
  return {
    send: (msg) => {
      if (socket?.readyState === WebSocket.OPEN) {
        socket.send(JSON.stringify(msg));
      }
    },
    dispose: () => {
      disposed = true;
      socket?.close();
    },
  };
};
