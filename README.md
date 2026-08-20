# não me prenda janja

Self-hosted screen sharing for Discord, packaged as a Discord Activity. One Rust binary: serves the activity UI, handles auth/authorization, and carries the video (up to 4K) through one of two selectable media backends — Cloudflare's Realtime SFU, or a built-in SFU where the binary itself relays the WebRTC media.

## How it works

```
Discord voice channel
 └─ Activity iframe (viewer grid, Discord-styled)
     │  fetch/wss via <app>.discordsays.com proxy
     ▼
 Rust binary (this repo) ── signaling, OAuth, authorization, share links
     │
     ├─ backend "cloudflare": HTTPS API (app secret stays here)
     │      ▼
     │  Cloudflare Realtime SFU ◄── WebRTC media ──► browsers
     │
     └─ backend "builtin": the binary IS the SFU
            ▲── WebRTC media (one UDP port) ──► browsers
```

- Discord's iframe doesn't allow `getDisplayMedia`, so **sharing** happens on an external page (`https://your-domain/share`) opened via the activity. The stream then appears inside the activity for everyone.
- **Watching** happens inside the activity iframe over WebRTC. Note: Discord's docs say WebRTC is "not supported" in activities — that refers to their proxy. The actual CSP on `discordsays.com` (verified) has no `webrtc 'block'` directive, so direct UDP media works today. This could break if Discord ever tightens the CSP.

### Choosing a backend

| | `cloudflare` | `builtin` |
| --- | --- | --- |
| Who relays media | Cloudflare Realtime SFU | this binary (RTP forwarded, never transcoded) |
| Cost | 1 TB/month egress free, then $0.05/GB | free — your server's bandwidth |
| Server bandwidth | ~zero (signaling only) | stream bitrate × (1 publisher in + N viewers out) |
| Extra setup | Cloudflare account + SFU app | one open UDP port |
| Networks blocking UDP | Cloudflare has TURN/TCP fallbacks | UDP only — those viewers can't connect |

Rule of thumb for `builtin`: a 20 Mbps 4K stream with 5 viewers ≈ 100 Mbps sustained egress, ~45 GB/hour. Fine on an unmetered VPS, painful on billed-per-GB clouds.

## Setup

### 1. Discord application ([developer portal](https://discord.com/developers/applications))

1. Create an application. Note the **Application ID** and (OAuth2 page) the **Client Secret**.
2. OAuth2 → Redirects: add a placeholder, e.g. `https://127.0.0.1` (required; the SDK handles the actual redirect).
3. Bot: create/reset the **Bot Token** (used server-side to verify activity participants; the bot needs no permissions and doesn't need to be in the guild).
4. Activities → Settings: enable Activities.
5. Activities → URL Mappings: set prefix `/` → target `your-domain.com` (no protocol).

### 2a. Media backend: Cloudflare Realtime

Dashboard → **Realtime → SFU** → create an app. Note the **App ID** and **App Secret**.

### 2b. Media backend: built-in SFU

Nothing to sign up for. You need:

- One **UDP port** (default `20101`) open in the firewall and reachable from the internet **directly on the machine** — WebRTC media does not go through the reverse proxy. All publishers and viewers share this single port.
- The server's **public IP**. Auto-detected at startup ([api.ipify.org](https://api.ipify.org)) or pinned via `public_ip` in the config.

### 3. Build & run

Requires Rust and [Bun](https://bun.sh) (frontend is built and embedded automatically).

```sh
cargo build --release
./target/release/nao-me-prenda-janja
```

First run asks for the values above (including which media backend) plus your public domain and port, then writes `config.toml` (mode 600 — keep it out of git). Subsequent runs just start the server. `--config <path>` to relocate.

To switch backends later, edit `config.toml`:

```toml
[rtc]
backend = "builtin"   # or "cloudflare" (the default when the key is absent)

[rtc.builtin]
udp_port = 20101
# public_ip = "203.0.113.7"   # optional — auto-detected when absent
```

### 4. Reverse proxy

Point `your-domain.com` (same domain as the URL mapping) at the configured port. TLS terminates at the proxy; WebSocket upgrade must be allowed on `/api/ws`. Generous idle timeouts recommended (the server pings every 30s). With the `builtin` backend the UDP media port bypasses the proxy entirely.

## Usage

Open the activity in a voice channel → **Share Your Screen** → browser tab opens (Discord shows a link confirmation — "Trust this Domain" hides it next time) → pick a screen/window → you're live in the activity. Click a tile to focus it, double-click for fullscreen, hover for mute/fullscreen controls. Multiple people can stream at once.

## Notes & limitations

- **4K**: capture requests 3840×2160@60 and caps the encoder at 20 Mbps, preferring AV1/VP9. Share a **monitor** for true 4K — Chromium captures tabs at logical (CSS) pixels, so a tab on a HiDPI display arrives at half resolution.
- **Audio**: tab audio works everywhere on Chromium; full system audio needs Chrome on Windows/ChromeOS (macOS since Chrome 141). Firefox can't capture audio at all, Safari is limited — share from Chrome/Edge.
- **Authorization**: every session (viewer and sharer) is validated server-side against Discord's activity-instance participant list — being outside the voice channel means no token, and websocket joins re-check. Share links are single-use codes that expire in 5 minutes. Backend and Discord secrets never reach the browser; track names are assigned server-side.
- **Static screens** (`cloudflare`): Cloudflare garbage-collects tracks after ~30s without packets; browsers send keepalive probes during screenshare so this is normally a non-issue, but a fully frozen source for a long time may need a re-share.
- **Builtin backend**: media is forwarded as-is (no simulcast, no server-side bandwidth adaptation) — viewers on connections slower than the publisher's bitrate will stutter; pick a lower resolution preset when sharing. No TURN/TCP fallback: viewers on UDP-blocking networks can't connect.
