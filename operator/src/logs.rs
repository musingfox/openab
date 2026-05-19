use anyhow::{Context, Result};

pub async fn run(
    aws_config: &aws_config::SdkConfig,
    name: &str,
    cluster: &str,
    namespace: &str,
    follow: bool,
    tail: i32,
) -> Result<()> {
    let ecs = aws_sdk_ecs::Client::new(aws_config);
    let logs_client = aws_sdk_cloudwatchlogs::Client::new(aws_config);

    let service_name = format!("oab-{}-{}", namespace, name);

    // 1. Find the running task for this service
    let tasks = ecs
        .list_tasks()
        .cluster(cluster)
        .service_name(&service_name)
        .send()
        .await
        .context("failed to list tasks")?;

    let task_arn = tasks
        .task_arns()
        .first()
        .context(format!("no running tasks found for {}", name))?;

    // 2. Get task details to find the log group/stream
    let task_desc = ecs
        .describe_tasks()
        .cluster(cluster)
        .tasks(task_arn)
        .send()
        .await
        .context("failed to describe task")?;

    let task = task_desc
        .tasks()
        .first()
        .context("task not found")?;

    // Extract task ID from ARN (last segment after /)
    let task_id = task_arn.rsplit('/').next().unwrap_or(task_arn);

    // ECS Fargate default log configuration: /ecs/{task-def-family}
    // Log stream: {container-name}/{task-id}
    let log_group = format!("/ecs/{}", service_name);
    let log_stream = format!("openab/{}", task_id);

    // Try the default awslogs pattern first, fall back to task def config
    let (final_group, final_stream) = match get_log_config(task) {
        Some((g, s)) => (g, s.replace("{task_id}", task_id)),
        None => (log_group, log_stream),
    };

    println!("📋 Logs for {} (task {})\n", name, &task_id[..8.min(task_id.len())]);

    // 3. Fetch logs
    let mut next_token: Option<String> = None;

    loop {
        let mut req = logs_client
            .get_log_events()
            .log_group_name(&final_group)
            .log_stream_name(&final_stream)
            .start_from_head(false)
            .limit(tail);

        if let Some(ref token) = next_token {
            req = req.next_token(token);
        }

        let resp = req.send().await;

        match resp {
            Ok(output) => {
                for event in output.events() {
                    if let Some(msg) = event.message() {
                        let ts = event.timestamp().unwrap_or(0);
                        let dt = format_timestamp(ts);
                        println!("{} {}", dt, msg);
                    }
                }

                if follow {
                    next_token = output.next_forward_token().map(|s| s.to_string());
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                } else {
                    break;
                }
            }
            Err(e) => {
                anyhow::bail!(
                    "failed to get logs (group={}, stream={}): {}",
                    final_group,
                    final_stream,
                    e
                );
            }
        }
    }

    Ok(())
}

fn get_log_config(task: &aws_sdk_ecs::types::Task) -> Option<(String, String)> {
    // Try to extract log configuration from the task's container overrides or definition
    // For now, return None and use the default pattern
    let _ = task;
    None
}

fn format_timestamp(millis: i64) -> String {
    let secs = millis / 1000;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", hours, mins, s)
}
