use std::io::{self, IsTerminal, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    pub discord: DiscordConfig,
    /// Required only when `rtc.backend = "cloudflare"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloudflare: Option<CloudflareConfig>,
    /// Which media backend carries the video. Absent in old configs, which
    /// predate the builtin SFU and always used Cloudflare.
    #[serde(default)]
    pub rtc: RtcConfig,
    pub server: ServerConfig,
    pub auth: AuthConfig,
}

impl Config {
    /// Present iff the Cloudflare backend is selected (validated at load).
    pub fn cloudflare(&self) -> &CloudflareConfig {
        self.cloudflare.as_ref().expect("cloudflare config validated at load")
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DiscordConfig {
    /// Application (client) ID from the Discord developer portal.
    pub client_id: String,
    pub client_secret: String,
    /// Bot token — used server-side to verify activity-instance participants.
    pub bot_token: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct CloudflareConfig {
    /// Cloudflare Realtime (SFU) App ID.
    pub app_id: String,
    /// Cloudflare Realtime App secret (bearer token for the SFU HTTPS API).
    pub app_secret: String,
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct RtcConfig {
    #[serde(default)]
    pub backend: RtcBackend,
    #[serde(default)]
    pub builtin: BuiltinRtcConfig,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RtcBackend {
    /// Media relayed by Cloudflare Realtime — free 1 TB/month egress, media
    /// never touches this server.
    #[default]
    Cloudflare,
    /// This binary is the SFU: media is proxied through it. Free, but all
    /// stream bandwidth (bitrate x viewers) flows through the server.
    Builtin,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct BuiltinRtcConfig {
    /// Single UDP port carrying all WebRTC media (must be open to the internet).
    #[serde(default = "default_udp_port")]
    pub udp_port: u16,
    /// Public IP advertised to browsers. Auto-detected at startup when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_ip: Option<String>,
}

fn default_udp_port() -> u16 {
    20101
}

impl Default for BuiltinRtcConfig {
    fn default() -> Self {
        Self { udp_port: default_udp_port(), public_ip: None }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub port: u16,
    /// Public domain the reverse proxy points at this binary, e.g. "activity.example.com".
    pub public_domain: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Hex-encoded HMAC key for session tokens. Generated on first run.
    pub session_secret: String,
}

pub fn load_or_setup(path: &Path) -> Result<Config> {
    if path.exists() {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw).context("parsing config file")?;
        if cfg.rtc.backend == RtcBackend::Cloudflare && cfg.cloudflare.is_none() {
            bail!(
                "rtc.backend is \"cloudflare\" but the [cloudflare] section is missing.\n\
                 Add app_id/app_secret, or switch to the builtin SFU with:\n\
                 [rtc]\nbackend = \"builtin\""
            );
        }
        return Ok(cfg);
    }

    if !io::stdin().is_terminal() {
        bail!(
            "no config at {} and stdin is not a terminal.\n\
             Run interactively once to generate it, or create the file yourself \
             (sections: [discord] client_id/client_secret/bot_token, \
             [rtc] backend = \"cloudflare\"|\"builtin\", \
             [cloudflare] app_id/app_secret (cloudflare backend), \
             [rtc.builtin] udp_port/public_ip (builtin backend), \
             [server] port/public_domain, [auth] session_secret).",
            path.display()
        );
    }

    println!("No config found — first-run setup. Values are saved to {}\n", path.display());
    println!("Discord app: https://discord.com/developers/applications (create one, enable Activities)");
    let client_id = ask("Discord application (client) ID")?;
    let client_secret = ask("Discord client secret (OAuth2 page)")?;
    let bot_token = ask("Discord bot token (Bot page — used to verify activity participants)")?;

    println!("\nVideo backend — who carries the stream data:");
    println!("  1) cloudflare — Cloudflare Realtime relays media (1 TB/month free egress, then $0.05/GB; media never touches this server)");
    println!("  2) builtin    — this binary is the SFU (no third party, but all stream bandwidth flows through this server; needs one open UDP port)");
    let backend = loop {
        match ask_default("Backend", "1")?.as_str() {
            "1" | "cloudflare" => break RtcBackend::Cloudflare,
            "2" | "builtin" => break RtcBackend::Builtin,
            _ => println!("Enter 1 or 2."),
        }
    };

    let mut cloudflare = None;
    let mut builtin = BuiltinRtcConfig::default();
    match backend {
        RtcBackend::Cloudflare => {
            println!("\nCloudflare Realtime: https://dash.cloudflare.com -> Realtime -> create an SFU app");
            let app_id = ask("Cloudflare Realtime app ID")?;
            let app_secret = ask("Cloudflare Realtime app secret")?;
            cloudflare = Some(CloudflareConfig { app_id, app_secret });
        }
        RtcBackend::Builtin => {
            println!();
            builtin.udp_port = ask_port(
                "UDP media port (must be reachable from the internet, UDP open in the firewall)",
                "20101",
            )?;
            let ip = ask_default("Public IP (empty = auto-detect at startup)", "")?;
            builtin.public_ip = (!ip.is_empty()).then_some(ip);
        }
    }

    println!();
    let public_domain = ask("Public domain (reverse-proxied to this binary, e.g. activity.example.com)")?;
    let port = ask_port("Port to listen on", "8787")?;

    let mut key = [0u8; 32];
    rand::rng().fill_bytes(&mut key);
    let session_secret = hex(&key);

    let cfg = Config {
        discord: DiscordConfig { client_id, client_secret, bot_token },
        cloudflare,
        rtc: RtcConfig { backend, builtin },
        server: ServerConfig { port, public_domain },
        auth: AuthConfig { session_secret },
    };

    let raw = toml::to_string_pretty(&cfg)?;
    std::fs::write(path, raw).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    println!("\nSaved {} (mode 600 — contains secrets, keep it out of git).", path.display());
    Ok(cfg)
}

fn ask(prompt: &str) -> Result<String> {
    loop {
        print!("{prompt}: ");
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        let value = line.trim().to_string();
        if !value.is_empty() {
            return Ok(value);
        }
        println!("Required — please enter a value.");
    }
}

/// A typo shouldn't abort setup and throw away everything entered so far.
fn ask_port(prompt: &str, default: &str) -> Result<u16> {
    loop {
        match ask_default(prompt, default)?.parse::<u16>() {
            Ok(port) => return Ok(port),
            Err(_) => println!("Not a valid port (1-65535) — try again."),
        }
    }
}

fn ask_default(prompt: &str, default: &str) -> Result<String> {
    print!("{prompt} [{default}]: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let value = line.trim();
    Ok(if value.is_empty() { default.to_string() } else { value.to_string() })
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
