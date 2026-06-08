use std::collections::HashMap;
use tracing::{error, info};

use crate::config::SecretsConfig;

/// Resolved secrets: mapping from key name to plaintext value.
pub type ResolvedSecrets = HashMap<String, String>;

/// Resolve all secret references in the [secrets] config table.
/// Returns a map of key → resolved value.
pub async fn resolve(cfg: &SecretsConfig) -> anyhow::Result<ResolvedSecrets> {
    let mut resolved = HashMap::new();

    for (key, uri) in &cfg.refs {
        let value = resolve_one(key, uri, cfg).await?;
        resolved.insert(key.clone(), value);
    }

    if !resolved.is_empty() {
        info!(count = resolved.len(), "secrets resolved");
    }
    Ok(resolved)
}

async fn resolve_one(key: &str, uri: &str, cfg: &SecretsConfig) -> anyhow::Result<String> {
    if uri.starts_with("aws-sm://") {
        #[cfg(feature = "secrets-aws")]
        {
            return resolve_aws_sm(key, uri, cfg).await;
        }
        #[cfg(not(feature = "secrets-aws"))]
        {
            anyhow::bail!(
                "secret '{key}' uses aws-sm:// but the 'secrets-aws' feature is not enabled"
            );
        }
    }

    if uri.starts_with("exec://") {
        return resolve_exec(key, uri, cfg).await;
    }

    anyhow::bail!(
        "secret '{key}': unrecognized URI scheme in '{uri}' (expected aws-sm:// or exec://)"
    );
}

// -- AWS Secrets Manager provider --

#[cfg(feature = "secrets-aws")]
async fn resolve_aws_sm(key: &str, uri: &str, cfg: &SecretsConfig) -> anyhow::Result<String> {
    let (secret_id, json_key) = parse_aws_sm_uri(uri)
        .ok_or_else(|| anyhow::anyhow!("secret '{key}': invalid aws-sm:// URI '{uri}' — expected aws-sm://<secret-id>#<json-key>"))?;

    let mut config_loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
    if let Some(ref region) = cfg.aws.region {
        config_loader = config_loader.region(aws_config::Region::new(region.clone()));
    }
    if let Some(ref endpoint) = cfg.aws.endpoint_url {
        config_loader = config_loader.endpoint_url(endpoint);
    }
    let sdk_config = config_loader.load().await;
    let client = aws_sdk_secretsmanager::Client::new(&sdk_config);

    let resp = client
        .get_secret_value()
        .secret_id(&secret_id)
        .send()
        .await
        .map_err(|e| {
            error!(secret = key, secret_id = %secret_id, "AWS Secrets Manager error");
            anyhow::anyhow!("secret '{key}': failed to fetch '{secret_id}' from AWS Secrets Manager: {e}")
        })?;

    let secret_string = resp
        .secret_string()
        .ok_or_else(|| anyhow::anyhow!("secret '{key}': '{secret_id}' has no string value (binary secrets not supported)"))?;

    // Parse as JSON and extract the key
    let json: serde_json::Value = serde_json::from_str(secret_string)
        .map_err(|e| anyhow::anyhow!("secret '{key}': '{secret_id}' is not valid JSON: {e}"))?;

    let value = json
        .get(&json_key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("secret '{key}': JSON key '{json_key}' not found in '{secret_id}'"))?;

    Ok(value.to_owned())
}

/// Parse `aws-sm://secret-id#json-key` into (secret_id, json_key).
#[cfg(feature = "secrets-aws")]
fn parse_aws_sm_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix("aws-sm://")?;
    let (secret_id, json_key) = rest.rsplit_once('#')?;
    if secret_id.is_empty() || json_key.is_empty() {
        return None;
    }
    Some((secret_id.to_owned(), json_key.to_owned()))
}

// -- Exec provider --

async fn resolve_exec(key: &str, uri: &str, cfg: &SecretsConfig) -> anyhow::Result<String> {
    let rest = uri.strip_prefix("exec://").unwrap();
    let mut parts_iter = rest.splitn(3, ' ');
    let script = parts_iter.next().ok_or_else(|| {
        anyhow::anyhow!("secret '{key}': exec:// URI missing script path")
    })?;
    if script.is_empty() {
        anyhow::bail!("secret '{key}': exec:// URI has empty script path");
    }

    let mut cmd = tokio::process::Command::new(script);
    cmd.kill_on_drop(true);
    // Pass remaining parts as arguments (key, attribute)
    for arg in parts_iter {
        if !arg.is_empty() {
            cmd.arg(arg);
        }
    }

    let timeout = std::time::Duration::from_secs(cfg.exec.timeout_seconds);
    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "secret '{key}': exec script '{script}' timed out after {}s",
                cfg.exec.timeout_seconds
            )
        })?
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!(
                    "secret '{key}': exec script '{script}' not found — did [hooks.pre_boot] run successfully?"
                )
            } else {
                anyhow::anyhow!("secret '{key}': failed to execute '{script}': {e}")
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        error!(secret = key, script, %stderr, "exec provider failed");
        anyhow::bail!(
            "secret '{key}': exec script '{script}' exited with {}",
            output.status
        );
    }

    let value = String::from_utf8(output.stdout)
        .map_err(|e| anyhow::anyhow!("secret '{key}': exec output is not valid UTF-8: {e}"))?;
    Ok(value.trim_end_matches('\n').to_owned())
}

/// Substitute `${secrets.<key>}` references in the raw config text with resolved values.
pub fn substitute(raw: &str, secrets: &ResolvedSecrets) -> String {
    let mut result = raw.to_owned();
    for (key, value) in secrets {
        let placeholder = format!("${{secrets.{key}}}");
        result = result.replace(&placeholder, value);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "secrets-aws")]
    #[test]
    fn parse_aws_sm_uri_valid() {
        let (id, key) = parse_aws_sm_uri("aws-sm://openab/prod#discord_bot_token").unwrap();
        assert_eq!(id, "openab/prod");
        assert_eq!(key, "discord_bot_token");
    }

    #[cfg(feature = "secrets-aws")]
    #[test]
    fn parse_aws_sm_uri_with_arn() {
        let uri = "aws-sm://arn:aws:secretsmanager:us-east-1:123456789:secret:my-secret-abc123#api_key";
        let (id, key) = parse_aws_sm_uri(uri).unwrap();
        assert_eq!(id, "arn:aws:secretsmanager:us-east-1:123456789:secret:my-secret-abc123");
        assert_eq!(key, "api_key");
    }

    #[cfg(feature = "secrets-aws")]
    #[test]
    fn parse_aws_sm_uri_missing_key() {
        assert!(parse_aws_sm_uri("aws-sm://openab/prod").is_none());
        assert!(parse_aws_sm_uri("aws-sm://openab/prod#").is_none());
        assert!(parse_aws_sm_uri("aws-sm://#key").is_none());
    }

    #[test]
    fn substitute_replaces_secrets() {
        let mut secrets = HashMap::new();
        secrets.insert("token".to_owned(), "my-secret-value".to_owned());
        let input = r#"bot_token = "${secrets.token}""#;
        let output = substitute(input, &secrets);
        assert_eq!(output, r#"bot_token = "my-secret-value""#);
    }

    #[test]
    fn substitute_no_match_unchanged() {
        let secrets = HashMap::new();
        let input = r#"bot_token = "${DISCORD_BOT_TOKEN}""#;
        let output = substitute(input, &secrets);
        assert_eq!(output, input);
    }
}
