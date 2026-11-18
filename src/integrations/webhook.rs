use serde_json::json;

pub async fn send_webhook_payload(
    client: &reqwest::Client,
    webhook_url: &str,
    message: &str,
) -> Result<(), String> {
    // Compatible with Discord ("content"), Slack ("text"), and generic webhooks simultaneously
    let payload = if webhook_url.contains("discord.com/api/webhooks") {
        json!({ "content": message })
    } else if webhook_url.contains("hooks.slack.com") {
        json!({ "text": message })
    } else {
        json!({ "content": message, "text": message })
    };

    let resp = client
        .post(webhook_url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("Webhook request error: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("Webhook returned HTTP {}", resp.status()));
    }
    Ok(())
}
