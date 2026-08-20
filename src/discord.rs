use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::state::AppState;

const API: &str = "https://discord.com/api/v10";

#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
}

#[derive(Deserialize)]
pub struct DiscordUser {
    pub id: String,
    pub username: String,
    pub global_name: Option<String>,
    pub avatar: Option<String>,
}

impl DiscordUser {
    pub fn display_name(&self) -> String {
        self.global_name.clone().unwrap_or_else(|| self.username.clone())
    }
}

/// Exchange the authorization code from the Embedded App SDK for an access token.
pub async fn exchange_code(state: &AppState, code: &str) -> Result<TokenResponse> {
    let resp = state
        .http
        .post(format!("{API}/oauth2/token"))
        .form(&[
            ("client_id", state.cfg.discord.client_id.as_str()),
            ("client_secret", state.cfg.discord.client_secret.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code),
        ])
        .send()
        .await
        .context("discord token exchange request")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("discord token exchange failed ({status}): {body}");
    }
    resp.json().await.context("discord token response parse")
}

pub async fn fetch_user(state: &AppState, access_token: &str) -> Result<DiscordUser> {
    let resp = state
        .http
        .get(format!("{API}/users/@me"))
        .bearer_auth(access_token)
        .send()
        .await
        .context("discord users/@me request")?;
    if !resp.status().is_success() {
        bail!("discord users/@me failed ({})", resp.status());
    }
    resp.json().await.context("discord user parse")
}

#[derive(Deserialize)]
struct ActivityInstance {
    #[serde(default)]
    users: Vec<String>,
    location: Option<ActivityLocation>,
}

#[derive(Deserialize)]
pub struct ActivityLocation {
    pub channel_id: Option<String>,
    pub guild_id: Option<String>,
}

pub struct InstanceCheck {
    pub is_participant: bool,
    pub location: Option<ActivityLocation>,
}

/// Verify via the Discord API (bot token) that `user_id` is currently a
/// participant of the given activity instance. This is the authorization
/// backbone: a valid OAuth user who is NOT in the activity's voice channel
/// cannot get a session for that room.
pub async fn check_instance_participant(
    state: &AppState,
    instance_id: &str,
    user_id: &str,
) -> Result<InstanceCheck> {
    let url = format!(
        "{API}/applications/{}/activity-instances/{instance_id}",
        state.cfg.discord.client_id
    );
    let resp = state
        .http
        .get(url)
        .header("Authorization", format!("Bot {}", state.cfg.discord.bot_token))
        .send()
        .await
        .context("discord activity-instance request")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(InstanceCheck { is_participant: false, location: None });
    }
    if !resp.status().is_success() {
        bail!("discord activity-instance failed ({})", resp.status());
    }
    let instance: ActivityInstance = resp.json().await.context("activity-instance parse")?;
    Ok(InstanceCheck {
        is_participant: instance.users.iter().any(|u| u == user_id),
        location: instance.location,
    })
}
