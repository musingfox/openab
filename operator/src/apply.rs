use crate::discord;
use crate::manifest::OABServiceManifest;
use anyhow::{Context, Result};
use aws_sdk_ecs::types::{
    AssignPublicIp, AwsVpcConfiguration, CapacityProviderStrategyItem, ContainerDefinition,
    KeyValuePair, NetworkConfiguration, Secret,
};
use aws_sdk_s3::primitives::ByteStream;
use std::path::Path;

pub async fn run(
    aws_config: &aws_config::SdkConfig,
    file_path: &str,
    auto_register: bool,
) -> Result<()> {
    let path = Path::new(file_path);
    let manifests = load_manifests(path)?;

    if manifests.is_empty() {
        anyhow::bail!("no manifests found at {}", file_path);
    }

    let ecs = aws_sdk_ecs::Client::new(aws_config);
    let s3 = aws_sdk_s3::Client::new(aws_config);
    let ssm = aws_sdk_ssm::Client::new(aws_config);

    // Load Discord developer token if auto-register is enabled
    let discord_token = if auto_register {
        Some(get_discord_developer_token(&ssm).await?)
    } else {
        None
    };

    let mut invite_urls: Vec<(String, String)> = Vec::new();

    for m in &manifests {
        m.validate()?;
        println!("  Applying {}...", m.metadata.name);

        let invite = apply_one(&ecs, &s3, &ssm, m, discord_token.as_deref()).await?;
        if let Some(url) = invite {
            invite_urls.push((m.metadata.name.clone(), url));
        }
    }

    println!("\n{} service(s) applied.", manifests.len());

    if !invite_urls.is_empty() {
        println!("\n📎 Discord Bot Invite URLs:");
        for (name, url) in &invite_urls {
            println!("  {} → {}", name, url);
        }
        println!("\nPaste these URLs into your browser to add bots to your Discord server.");
    }

    Ok(())
}

async fn get_discord_developer_token(ssm: &aws_sdk_ssm::Client) -> Result<String> {
    // Try env var first, then SSM
    if let Ok(token) = std::env::var("DISCORD_DEVELOPER_TOKEN") {
        return Ok(token);
    }

    let resp = ssm
        .get_parameter()
        .name("/oab/discord-developer-token")
        .with_decryption(true)
        .send()
        .await
        .context("failed to get Discord developer token from SSM (set DISCORD_DEVELOPER_TOKEN env var or store in SSM at /oab/discord-developer-token)")?;

    resp.parameter()
        .and_then(|p| p.value())
        .map(|v| v.to_string())
        .context("Discord developer token parameter has no value")
}

fn load_manifests(path: &Path) -> Result<Vec<OABServiceManifest>> {
    let mut manifests = Vec::new();
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "yaml" || e == "yml") {
                manifests.push(parse_manifest(&p)?);
            }
        }
    } else {
        manifests.push(parse_manifest(path)?);
    }
    Ok(manifests)
}

fn parse_manifest(path: &Path) -> Result<OABServiceManifest> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_yaml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

async fn apply_one(
    ecs: &aws_sdk_ecs::Client,
    s3: &aws_sdk_s3::Client,
    ssm: &aws_sdk_ssm::Client,
    m: &OABServiceManifest,
    discord_token: Option<&str>,
) -> Result<Option<String>> {
    let service_name = m.ecs_service_name();
    let bucket = "oab-control-plane";

    // Read current generation from S3 manifest (if exists), increment
    let manifest_key = format!(
        "manifests/{}/{}.yaml",
        m.metadata.namespace, m.metadata.name
    );
    let current_gen = match s3
        .get_object()
        .bucket(bucket)
        .key(&manifest_key)
        .send()
        .await
    {
        Ok(resp) => {
            let bytes = resp.body.collect().await?.into_bytes();
            let existing: OABServiceManifest = serde_yaml::from_slice(&bytes)?;
            existing.metadata.generation
        }
        Err(_) => 0,
    };
    let generation = current_gen + 1;

    // Auto-register Discord bot if --auto-register and no DISCORD_TOKEN secret defined
    let mut invite_url = None;
    let has_discord_secret = m.spec.secrets.iter().any(|s| s.name == "DISCORD_TOKEN");

    if let Some(token) = discord_token {
        if !has_discord_secret {
            let bot_name = format!("oab-{}-{}", m.metadata.namespace, m.metadata.name);
            let ssm_path = format!(
                "/oab/{}/{}/discord-token",
                m.metadata.namespace, m.metadata.name
            );

            // Check if token already exists in SSM (idempotent: skip if already provisioned)
            let already_provisioned = match ssm.get_parameter().name(&ssm_path).send().await {
                Ok(_) => true,
                Err(err) if err.as_service_error().is_some_and(is_parameter_not_found) => false,
                Err(err) => {
                    anyhow::bail!(
                        "failed to check existing Discord token at {}: {}",
                        ssm_path,
                        err
                    );
                }
            };

            if already_provisioned {
                println!(
                    "    ✓ Bot already provisioned (token exists at {})",
                    ssm_path
                );
            } else {
                println!("    Registering Discord bot '{}'...", bot_name);

                let bot = discord::provision_bot(token, &bot_name).await?;

                // Store token in SSM
                ssm.put_parameter()
                    .name(&ssm_path)
                    .value(&bot.bot_token)
                    .r#type(aws_sdk_ssm::types::ParameterType::SecureString)
                    .overwrite(true)
                    .send()
                    .await
                    .context("failed to store Discord bot token in SSM")?;

                println!("    ✓ Bot registered, token stored at {}", ssm_path);
                invite_url = Some(bot.invite_url);
            }
        }
    }

    // 1. Render config.toml and upload to S3 (immutable path)
    let config_toml = render_config_toml(&m.spec.config);
    let config_key = format!(
        "config/{}/{}/{}/config.toml",
        m.metadata.namespace, m.metadata.name, generation
    );
    s3.put_object()
        .bucket(bucket)
        .key(&config_key)
        .body(ByteStream::from(config_toml.into_bytes()))
        .send()
        .await
        .context("failed to upload config to S3")?;

    // 2. Upload manifest to S3 (record of desired state, with updated generation)
    let mut manifest_to_store = serde_yaml::to_value(m)?;
    manifest_to_store["metadata"]["generation"] = serde_yaml::Value::Number(generation.into());

    // Persist auto-registered secret in desired state so future applies without --auto-register keep it
    if discord_token.is_some() && !has_discord_secret {
        let ssm_path = format!(
            "/oab/{}/{}/discord-token",
            m.metadata.namespace, m.metadata.name
        );
        let secret_entry = serde_yaml::to_value(&crate::manifest::SecretRef {
            name: "DISCORD_TOKEN".to_string(),
            value_from: ssm_path,
        })?;
        if let Some(secrets) = manifest_to_store["spec"]["secrets"].as_sequence_mut() {
            secrets.push(secret_entry);
        } else {
            manifest_to_store["spec"]["secrets"] = serde_yaml::Value::Sequence(vec![secret_entry]);
        }
    }

    let manifest_yaml = serde_yaml::to_string(&manifest_to_store)?;
    let manifest_key = format!(
        "manifests/{}/{}.yaml",
        m.metadata.namespace, m.metadata.name
    );
    s3.put_object()
        .bucket(bucket)
        .key(&manifest_key)
        .body(ByteStream::from(manifest_yaml.into_bytes()))
        .send()
        .await
        .context("failed to upload manifest to S3")?;

    // 3. Register task definition
    let config_s3_path = format!("s3://{}/{}", bucket, config_key);
    let task_def_family = service_name.clone();

    let mut env_vars = vec![
        KeyValuePair::builder()
            .name("NAMESPACE")
            .value(&m.metadata.namespace)
            .build(),
        KeyValuePair::builder()
            .name("NAME")
            .value(&m.metadata.name)
            .build(),
        KeyValuePair::builder()
            .name("CONFIG_S3_PATH")
            .value(&config_s3_path)
            .build(),
    ];
    if let Some(ref bootstrap) = m.spec.bootstrap_from {
        env_vars.push(
            KeyValuePair::builder()
                .name("BOOTSTRAP_FROM")
                .value(bootstrap)
                .build(),
        );
    }

    let mut secrets: Vec<Secret> = m
        .spec
        .secrets
        .iter()
        .map(|s| {
            Secret::builder()
                .name(&s.name)
                .value_from(&s.value_from)
                .build()
                .unwrap()
        })
        .collect();

    // If auto-registered, add the Discord token secret to task def
    if discord_token.is_some() && !has_discord_secret {
        let ssm_path = format!(
            "/oab/{}/{}/discord-token",
            m.metadata.namespace, m.metadata.name
        );
        secrets.push(
            Secret::builder()
                .name("DISCORD_TOKEN")
                .value_from(&ssm_path)
                .build()
                .unwrap(),
        );
    }

    let container = ContainerDefinition::builder()
        .name("openab")
        .image(&m.spec.task_definition.image)
        .essential(true)
        .set_environment(Some(env_vars))
        .set_secrets(if secrets.is_empty() {
            None
        } else {
            Some(secrets)
        })
        .build();

    let task_def = ecs
        .register_task_definition()
        .family(&task_def_family)
        .requires_compatibilities(aws_sdk_ecs::types::Compatibility::Fargate)
        .network_mode(aws_sdk_ecs::types::NetworkMode::Awsvpc)
        .cpu(m.spec.cpu.to_string())
        .memory(m.spec.memory.to_string())
        .container_definitions(container)
        .send()
        .await
        .context("failed to register task definition")?;

    let task_def_arn = task_def
        .task_definition()
        .and_then(|td| td.task_definition_arn())
        .unwrap_or_default()
        .to_string();

    // 4. Create or update ECS service
    let assign_ip = if m.spec.networking.assign_public_ip {
        AssignPublicIp::Enabled
    } else {
        AssignPublicIp::Disabled
    };

    let vpc_config = AwsVpcConfiguration::builder()
        .set_subnets(Some(m.spec.networking.subnets.clone()))
        .set_security_groups(Some(m.spec.networking.security_groups.clone()))
        .assign_public_ip(assign_ip)
        .build()?;

    let network_config = NetworkConfiguration::builder()
        .awsvpc_configuration(vpc_config)
        .build();

    // Check if service exists
    let existing = ecs
        .describe_services()
        .cluster("default")
        .services(&service_name)
        .send()
        .await;

    let service_active = existing
        .as_ref()
        .ok()
        .and_then(|r| r.services().first())
        .is_some_and(|s| s.status() == Some("ACTIVE"));

    if service_active {
        // Update existing service
        ecs.update_service()
            .cluster("default")
            .service(&service_name)
            .task_definition(&task_def_arn)
            .network_configuration(network_config)
            .send()
            .await
            .context("failed to update ECS service")?;
        println!("  ✓ {} updated", m.metadata.name);
    } else {
        // Create new service
        let cap_strategy = CapacityProviderStrategyItem::builder()
            .capacity_provider(&m.spec.capacity_provider)
            .weight(1)
            .build()?;

        ecs.create_service()
            .cluster("default")
            .service_name(&service_name)
            .task_definition(&task_def_arn)
            .desired_count(1)
            .capacity_provider_strategy(cap_strategy)
            .network_configuration(network_config)
            .send()
            .await
            .context("failed to create ECS service")?;
        println!(
            "  ✓ {} created ({}, {}cpu/{}mem)",
            m.metadata.name, m.spec.capacity_provider, m.spec.cpu, m.spec.memory
        );
    }

    Ok(invite_url)
}

fn render_config_toml(config: &crate::manifest::AgentConfig) -> String {
    let mut out = String::new();

    if let Some(ref backend) = config.backend {
        out.push_str("[backend]\n");
        out.push_str(&format!("type = \"{}\"\n", backend.backend_type));
        if let Some(ref model) = backend.model_id {
            out.push_str(&format!("model_id = \"{}\"\n", model));
        }
        if let Some(ref region) = backend.region {
            out.push_str(&format!("region = \"{}\"\n", region));
        }
        out.push('\n');
    }

    for (i, ch) in config.channels.iter().enumerate() {
        out.push_str("[[channels]]\n");
        out.push_str(&format!("type = \"{}\"\n", ch.channel_type));
        for (k, v) in &ch.extra {
            if let serde_yaml::Value::String(s) = v {
                out.push_str(&format!("{} = \"{}\"\n", k, s));
            }
        }
        if i < config.channels.len() - 1 {
            out.push('\n');
        }
    }

    if let Some(ref steering) = config.steering {
        out.push_str("\n[steering]\n");
        if let Some(ref prompt) = steering.system_prompt {
            out.push_str(&format!("system_prompt = \"\"\"\n{}\n\"\"\"\n", prompt));
        }
    }

    if let Some(ref features) = config.features {
        out.push_str("\n[features]\n");
        out.push_str(&format!("stt = {}\n", features.stt));
        out.push_str(&format!("cronjob = {}\n", features.cronjob));
    }

    out
}

fn is_parameter_not_found(err: &aws_sdk_ssm::operation::get_parameter::GetParameterError) -> bool {
    err.is_parameter_not_found()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_ssm::{
        operation::get_parameter::GetParameterError, types::error::ParameterNotFound,
    };

    #[test]
    fn classifies_only_parameter_not_found_as_missing() {
        let missing = GetParameterError::ParameterNotFound(ParameterNotFound::builder().build());
        assert!(is_parameter_not_found(&missing));

        let other = GetParameterError::unhandled(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "ssm denied",
        ));
        assert!(!is_parameter_not_found(&other));
    }
}
