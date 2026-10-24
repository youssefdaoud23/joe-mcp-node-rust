use anyhow::{Result, anyhow};
use clap::Parser;
use joe_mcp_rust::{
    catalog,
    developer::DeveloperAccess,
    executor::{ExecPolicy, HostExecutor},
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{fs, process::Command};

#[derive(Parser, Debug)]
#[command(name = "joe-mcp-rust-agent", version)]
struct Args {
    #[arg(long, env = "JOE_RUST_SERVER")]
    server: String,
    #[arg(long, env = "JOE_RUST_DEVICE_ID", default_value = "")]
    device_id: String,
    #[arg(long, env = "JOE_RUST_DEVICE_TOKEN", default_value = "")]
    token: String,
    #[arg(long, env = "JOE_RUST_DEVICE_NAME", default_value = "")]
    name: String,
    #[arg(long, env = "JOE_RUST_ENROLL", default_value = "")]
    enroll: String,
    #[arg(
        long,
        env = "JOE_RUST_AGENT_CONFIG",
        default_value = "/etc/joe-mcp-rust-agent.json"
    )]
    config: String,
    #[arg(long, default_value_t = 20)]
    poll_seconds: u64,
    #[arg(long, default_value_t = false)]
    enroll_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct AgentConfig {
    server: String,
    device_id: String,
    token: String,
    name: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let args = Args::parse();
    let client = Client::builder()
        .timeout(Duration::from_secs(args.poll_seconds + 15))
        .build()?;
    let executor = HostExecutor::new(
        ExecPolicy::managed_agent_default(),
        PathBuf::from("/var/lib/joe-mcp-rust-agent/trash"),
    )?;
    let developer = DeveloperAccess::new(
        PathBuf::from("/var/lib/joe-mcp-rust-agent/developer-access.json"),
        executor.clone(),
    );
    developer.revoke_sudo("agent_start").await.ok();
    let mut cfg = load_or_build(&args).await?;
    if !args.enroll.is_empty() {
        let meta = metadata(
            if args.name.is_empty() {
                hostname()
            } else {
                args.name.clone()
            },
            &executor,
        );
        let response: Value = client
            .post(format!(
                "{}/api/device/register",
                args.server.trim_end_matches('/')
            ))
            .json(&json!({"enroll":args.enroll,"metadata":meta}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        cfg = AgentConfig {
            server: args.server.clone(),
            device_id: response["device_id"].as_str().unwrap_or_default().into(),
            token: response["device_token"].as_str().unwrap_or_default().into(),
            name: if args.name.is_empty() {
                hostname()
            } else {
                args.name.clone()
            },
        };
        if cfg.device_id.is_empty() || cfg.token.is_empty() {
            return Err(anyhow!("controller returned incomplete enrollment"));
        }
        save_config(&args.config, &cfg).await?;
        tracing::info!(device_id=%cfg.device_id,"Rust agent enrolled");
        if args.enroll_only {
            return Ok(());
        }
    }
    if cfg.server.is_empty() || cfg.device_id.is_empty() || cfg.token.is_empty() {
        return Err(anyhow!("agent is not enrolled"));
    }

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {result=run_loop(&client,&cfg,args.poll_seconds,executor.clone(),developer.clone(),&args.config)=>result?,_=terminate.recv()=>tracing::info!("termination requested"),_=tokio::signal::ctrl_c()=>tracing::info!("interrupt requested")}
    }
    #[cfg(not(unix))]
    {
        tokio::select! {result=run_loop(&client,&cfg,args.poll_seconds,executor.clone(),developer.clone(),&args.config)=>result?,_=tokio::signal::ctrl_c()=>tracing::info!("interrupt requested")}
    }
    report_offline(&client, &cfg).await;
    Ok(())
}

async fn run_loop(
    client: &Client,
    cfg: &AgentConfig,
    poll_seconds: u64,
    executor: HostExecutor,
    developer: DeveloperAccess,
    config_path: &str,
) -> Result<()> {
    let mut cfg = cfg.clone();
    let poll_url = format!("{}/api/device/poll", cfg.server.trim_end_matches('/'));
    let result_url = format!("{}/api/device/result", cfg.server.trim_end_matches('/'));
    loop {
        let meta = metadata(
            if cfg.name.is_empty() {
                hostname()
            } else {
                cfg.name.clone()
            },
            &executor,
        );
        let response = client
            .post(&poll_url)
            .header("Authorization", format!("Device {}", cfg.token))
            .json(&json!({"metadata":meta,"wait_seconds":poll_seconds.min(25)}))
            .send()
            .await;
        let response = match response {
            Ok(r) => match r.error_for_status() {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error=%e,"poll failed");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            },
            Err(e) => {
                tracing::warn!(error=%e,"poll transport failed");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };
        let payload: Value = match response.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error=%e,"invalid poll response");
                continue;
            }
        };
        if let Some(policy) = payload.get("developer_access")
            && let Err(e) = developer.configure(policy).await
        {
            tracing::warn!(error=%e,"Developer Access policy sync failed closed");
        }
        let filesystem_policy = payload
            .get("filesystem_policy")
            .cloned()
            .unwrap_or_else(|| json!({"mode":"restricted","roots":[]}));
        let remote_read_enabled = payload
            .get("remote_read_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let remote_action_enabled = payload
            .get("remote_action_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let Some(job) = payload.get("job").filter(|v| !v.is_null()) else {
            continue;
        };
        let job_id = job
            .get("job_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let tool = job
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut arguments = job.get("arguments").cloned().unwrap_or(json!({}));
        if job_id.is_empty() {
            continue;
        }
        let shutdown = tool == "__shutdown_managed_agent__";
        let outcome = if shutdown {
            Ok(json!({"accepted":true,"shutdown":true}))
        } else if tool == "__rotate_device_token__" {
            rotate_device_token(client, &mut cfg, config_path).await
        } else if tool == "__update_managed_agent__" {
            trigger_managed_update().await
        } else if tool == "managed_update_status" {
            managed_update_status().await
        } else if tool == "__developer_access_configure__" {
            developer
                .configure(
                    arguments
                        .get("policy")
                        .or_else(|| arguments.get("developer_access"))
                        .unwrap_or(&json!({})),
                )
                .await
        } else if tool == "__developer_access_authorize_sudo__" {
            let key = ["sudo", "password"].join("_");
            let ttl = arguments
                .get("ttl_minutes")
                .and_then(Value::as_i64)
                .unwrap_or(15);
            let secret = arguments
                .as_object_mut()
                .and_then(|obj| obj.remove(&key))
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default();
            let result = developer.authorize_sudo(&secret, ttl, true).await;
            drop(secret);
            result
        } else if tool == "__developer_access_revoke_sudo__" {
            developer
                .revoke_sudo(
                    arguments
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("revoked"),
                )
                .await
        } else if tool == "developer_access_status" {
            developer.status().await
        } else if tool == "start_developer_terminal" {
            developer.start_terminal(&arguments).await
        } else if catalog::is_read_tool(&tool) && !remote_read_enabled {
            Err(anyhow!("Remote reads are disabled by this device policy"))
        } else if catalog::is_action_tool(&tool) && !remote_action_enabled {
            Err(anyhow!("Remote actions are disabled by this device policy"))
        } else if let Err(e) = enforce_filesystem_scope(&filesystem_policy, &tool, &arguments) {
            Err(e)
        } else {
            executor.execute(&tool, arguments).await
        };
        let body = match outcome {
            Ok(result) => json!({"job_id":job_id,"result":result,"error":null}),
            Err(e) => json!({"job_id":job_id,"result":null,"error":e.to_string()}),
        };
        if let Err(e) = submit_result_with_retry(client, &cfg, &result_url, &body).await {
            tracing::warn!(error=%e,"result submission failed after bounded retries");
        }
        if shutdown {
            report_offline(client, &cfg).await;
            return Ok(());
        }
    }
}

async fn submit_result_with_retry(
    client: &Client,
    cfg: &AgentConfig,
    result_url: &str,
    body: &Value,
) -> Result<()> {
    const ATTEMPTS: u32 = 5;
    let mut last_error = None;
    for attempt in 0..ATTEMPTS {
        match client
            .post(result_url)
            .header("Authorization", format!("Device {}", cfg.token))
            .json(body)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                let status = response.status();
                let error = anyhow!("result submission rejected with HTTP {status}");
                if !(status.is_server_error()
                    || status == reqwest::StatusCode::REQUEST_TIMEOUT
                    || status == reqwest::StatusCode::TOO_MANY_REQUESTS)
                {
                    return Err(error);
                }
                last_error = Some(error);
            }
            Err(error) => {
                last_error = Some(anyhow!("result submission transport failed: {error}"));
            }
        }
        if attempt + 1 < ATTEMPTS {
            let delay_ms = 200u64.saturating_mul(1u64 << attempt.min(4));
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("result submission failed")))
}

async fn rotate_device_token(
    client: &Client,
    cfg: &mut AgentConfig,
    config_path: &str,
) -> Result<Value> {
    let response: Value = client
        .post(format!(
            "{}/api/device/rotate",
            cfg.server.trim_end_matches('/')
        ))
        .header("Authorization", format!("Device {}", cfg.token))
        .json(&json!({"device_id":cfg.device_id}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let key = ["device", "token"].join("_");
    let replacement = response
        .get(&key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("controller returned an incomplete rotated credential"))?
        .to_string();
    cfg.token = replacement;
    save_config(config_path, cfg).await?;
    Ok(
        json!({"rotated":true,"device_id":cfg.device_id,"grace_seconds":response.get("grace_seconds").and_then(Value::as_u64).unwrap_or(0)}),
    )
}

async fn trigger_managed_update() -> Result<Value> {
    let service = "joe-mcp-rust-update.service";
    let output = Command::new("systemctl")
        .args(["start", "--no-block", service])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;
    if !output.success() {
        return Err(anyhow!("Rust managed update service could not be started"));
    }
    Ok(json!({"update_requested":true,"service":service,"status_tool":"managed_update_status"}))
}

async fn managed_update_status() -> Result<Value> {
    let path = Path::new("/var/lib/joe-mcp-rust-agent/update-status.json");
    match fs::read(path).await {
        Ok(data) => Ok(serde_json::from_slice(&data)
            .unwrap_or_else(|_| json!({"state":"invalid","error":"update status is unreadable"}))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(
            json!({"state":"idle","installed_version":env!("CARGO_PKG_VERSION"),"runtime":"rust"}),
        ),
        Err(e) => Err(e.into()),
    }
}

async fn report_offline(client: &Client, cfg: &AgentConfig) {
    let url = format!("{}/api/device/offline", cfg.server.trim_end_matches('/'));
    match client
        .post(url)
        .header("Authorization", format!("Device {}", cfg.token))
        .json(&json!({"reason":"agent_shutdown"}))
        .timeout(Duration::from_secs(8))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => tracing::info!("device marked offline"),
        Ok(response) => tracing::warn!(status=%response.status(),"offline report rejected"),
        Err(error) => tracing::warn!(%error,"offline report failed; controller TTL remains active"),
    }
}

fn metadata(name: String, executor: &HostExecutor) -> Value {
    json!({"name":name,"hostname":hostname(),"os":std::env::consts::OS,"agent_version":env!("CARGO_PKG_VERSION"),"software_version":env!("CARGO_PKG_VERSION"),"architecture":std::env::consts::ARCH,"routing_version":"1","filesystem_scope_version":"1","developer_access_version":"1","update_lifecycle":Path::new("/usr/local/libexec/joe-mcp-rust-updater").is_file(),"shutdown_lifecycle":true,"rust_policy":executor.policy_view()})
}
fn enforce_filesystem_scope(policy: &Value, tool: &str, args: &Value) -> Result<()> {
    let mode = policy
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("full_host");
    if mode == "full_host" {
        return Ok(());
    }
    if mode != "restricted" {
        return Err(anyhow!("Unsupported filesystem policy mode"));
    }
    let roots = policy
        .get("roots")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Restricted filesystem policy requires roots"))?;
    if roots.is_empty() {
        return Err(anyhow!("Restricted filesystem policy has no roots"));
    }
    if matches!(
        tool,
        "start_process"
            | "start_workspace_shell"
            | "start_managed_terminal"
            | "start_workspace_python"
            | "start_workspace_python_code"
            | "start_workspace_node_check"
            | "start_workspace_pytest"
    ) {
        return Err(anyhow!(
            "Raw execution is disabled while restricted filesystem scope is active"
        ));
    }
    match tool {
        "copy_file" => {
            scope_check(roots, required_arg_path(args, "source")?, false)?;
            scope_check(roots, required_arg_path(args, "destination")?, true)?;
        }
        "move_file" => {
            scope_check(roots, required_arg_path(args, "source")?, true)?;
            scope_check(roots, required_arg_path(args, "destination")?, true)?;
        }
        "write_file" | "edit_file" | "edit_block" | "create_directory" | "trash_path"
        | "restore_trash" | "purge_trash" => {
            if let Some(p) = args.get("path").and_then(Value::as_str) {
                scope_check(roots, p, true)?;
            }
        }
        name if name.starts_with("git_") => {
            if let Some(p) = args
                .get("path")
                .or_else(|| args.get("repo"))
                .and_then(Value::as_str)
            {
                scope_check(roots, p, catalog::is_action_tool(name))?;
            }
        }
        "sqlite_tables" | "sqlite_schema" | "sqlite_query" | "sqlite_table_stats" => {
            if let Some(p) = args
                .get("database")
                .or_else(|| args.get("path"))
                .and_then(Value::as_str)
            {
                scope_check(roots, p, false)?;
            }
        }
        _ => {
            for key in ["path", "root", "cwd", "script"] {
                if let Some(p) = args.get(key).and_then(Value::as_str) {
                    scope_check(roots, p, catalog::is_action_tool(tool))?;
                }
            }
        }
    }
    Ok(())
}
fn required_arg_path<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{key} is required"))
}
fn scope_check(roots: &[Value], raw: &str, write: bool) -> Result<()> {
    let candidate = scope_resolve(Path::new(raw))?;
    for root in roots {
        let Some(root_path) = root.get("path").and_then(Value::as_str) else {
            continue;
        };
        let access = root
            .get("access")
            .and_then(Value::as_str)
            .unwrap_or("read_only");
        let Ok(resolved_root) = std::fs::canonicalize(root_path) else {
            continue;
        };
        if candidate.starts_with(&resolved_root) && (!write || access == "read_write") {
            return Ok(());
        }
    }
    Err(anyhow!(if write {
        "Filesystem write is outside the device's allowed roots"
    } else {
        "Filesystem read is outside the device's allowed roots"
    }))
}
fn scope_resolve(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(anyhow!("Filesystem scope paths must be absolute"));
    }
    if let Ok(p) = std::fs::canonicalize(path) {
        return Ok(p);
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Filesystem path has no parent"))?;
    let canonical_parent = std::fs::canonicalize(parent)
        .map_err(|_| anyhow!("Filesystem path parent does not exist"))?;
    Ok(canonical_parent.join(
        path.file_name()
            .ok_or_else(|| anyhow!("Filesystem path is invalid"))?,
    ))
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .unwrap_or_else(|_| "rust-node".into())
        .trim()
        .to_string()
}
async fn load_or_build(args: &Args) -> Result<AgentConfig> {
    if let Ok(raw) = fs::read_to_string(&args.config).await
        && let Ok(cfg) = serde_json::from_str::<AgentConfig>(&raw)
    {
        return Ok(cfg);
    }
    Ok(AgentConfig {
        server: args.server.clone(),
        device_id: args.device_id.clone(),
        token: args.token.clone(),
        name: args.name.clone(),
    })
}
async fn save_config(path: &str, cfg: &AgentConfig) -> Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(path, serde_json::to_vec_pretty(cfg)?).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[tokio::test]
    async fn result_submission_retries_transient_controller_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for status in ["503 Service Unavailable", "200 OK"] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = vec![0u8; 8192];
                let _ = stream.read(&mut request).await.unwrap();
                let response =
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });

        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let cfg = AgentConfig {
            server: format!("http://{address}"),
            device_id: "dev_test".into(),
            token: "device-test-token".into(),
            name: "test".into(),
        };
        submit_result_with_retry(
            &client,
            &cfg,
            &format!("http://{address}/api/device/result"),
            &json!({"job_id":"job_test","result":{"ok":true},"error":null}),
        )
        .await
        .unwrap();

        server.await.unwrap();
    }
}
