export type Share = {
  id: string;
  user_id: string;
  username: string;
  avatar: string | null;
  has_audio: boolean;
  started_at: number;
};

export type PublicUser = {
  id: string;
  name: string;
  avatar: string | null;
};

export type ServerMsg =
  | { type: "hello"; conn_id: number; shares: Share[] }
  | { type: "share_started"; share: Share }
  | { type: "share_ended"; share_id: string }
  | { type: "auth_failed" };

export const avatarUrl = (userId: string, avatar: string | null, size = 128) => {
  if (avatar) {
    return `https://cdn.discordapp.com/avatars/${userId}/${avatar}.png?size=${size}`;
  }
  const index = Number(BigInt(userId) >> 22n) % 6;
  return `https://cdn.discordapp.com/embed/avatars/${index}.png`;
};
