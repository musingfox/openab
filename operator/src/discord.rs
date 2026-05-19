use anyhow::{Context, Result};
use serde::Deserialize;

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const BOT_PERMISSIONS: u64 = 274878221312; // Send Messages, Read Messages, etc.

#[derive(Debug)]
pub struct ProvisionedBot {
    pub application_id: String,
    pub bot_token: String,
    pub invite_url: String,
}

#[derive(Deserialize)]
struct ApplicationResponse {
    id: String,
    #[allow(dead_code)]
    name: String,
}

#[derive(Deserialize)]
struct BotResponse {
    #[allow(dead_code)]
    id: String,
    token: Option<String>,
}

/// Provision a Discord bot application and return the bot token + invite URL.
/// Requires a Discord user bearer token with `applications.commands` scope.
///
/// Idempotency: if a bot with the same name already exists, we reset its token
/// rather than creating a duplicate.
pub async fn provision_bot(
    discord_token: &str,
    bot_name: &str,
) -> Result<ProvisionedBot> {
    let client = reqwest::Client::new();

    // 1. Check if application already exists (by listing user's apps)
    let existing = find_existing_application(&client, discord_token, bot_name).await?;

    let app_id = if let Some(app_id) = existing {
        app_id
    } else {
        // 2. Create new application
        let resp = client
            .post(format!("{}/applications", DISCORD_API_BASE))
            .header("Authorization", format!("Bearer {}", discord_token))
            .json(&serde_json::json!({ "name": bot_name }))
            .send()
            .await
            .context("failed to create Discord application")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Discord API error creating application: {} {}", status, body);
        }

        let app: ApplicationResponse = resp.json().await?;
        app.id
    };

    // 3. Create or reset bot user for the application
    let resp = client
        .post(format!("{}/applications/{}/bot/reset", DISCORD_API_BASE, app_id))
        .header("Authorization", format!("Bearer {}", discord_token))
        .send()
        .await
        .context("failed to reset Discord bot token")?;

    let bot_token = if resp.status().is_success() {
        let bot: BotResponse = resp.json().await?;
        bot.token.context("Discord API did not return a bot token")?
    } else {
        // If reset fails, try creating the bot first
        let resp = client
            .post(format!("{}/applications/{}/bot", DISCORD_API_BASE, app_id))
            .header("Authorization", format!("Bearer {}", discord_token))
            .send()
            .await
            .context("failed to create Discord bot")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Discord API error creating bot: {} {}", status, body);
        }

        let bot: BotResponse = resp.json().await?;
        bot.token.context("Discord API did not return a bot token")?
    };

    let invite_url = format!(
        "https://discord.com/oauth2/authorize?client_id={}&scope=bot&permissions={}",
        app_id, BOT_PERMISSIONS
    );

    Ok(ProvisionedBot {
        application_id: app_id,
        bot_token,
        invite_url,
    })
}

async fn find_existing_application(
    client: &reqwest::Client,
    discord_token: &str,
    bot_name: &str,
) -> Result<Option<String>> {
    let resp = client
        .get(format!("{}/applications", DISCORD_API_BASE))
        .header("Authorization", format!("Bearer {}", discord_token))
        .send()
        .await
        .context("failed to list Discord applications")?;

    if !resp.status().is_success() {
        // If we can't list, assume it doesn't exist
        return Ok(None);
    }

    let apps: Vec<ApplicationResponse> = resp.json().await?;
    Ok(apps.into_iter().find(|a| a.name == bot_name).map(|a| a.id))
}
