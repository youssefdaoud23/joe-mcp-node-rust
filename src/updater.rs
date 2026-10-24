use anyhow::{Result, anyhow, bail};
use clap::Parser;
use reqwest::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{fs, process::Command};

#[derive(Parser, Debug)]
#[command(name = "joe-mcp-rust-updater", version)]
struct Args {
    #[arg(long, env = "JOE_RUST_SERVER")]
    server: String,
    #[arg(long, default_value = "/usr/local/libexec/joe-mcp-rust-agent")]
    agent: PathBuf,
    #[arg(long, default_value = "/usr/local/libexec/joe-mcp-rust-updater")]
    updater: PathBuf,
    #[arg(long, default_value = "joe-mcp-rust-agent.service")]
    service: String,
    #[arg(long, default_value = "/var/lib/joe-mcp-rust-agent/update-status.json")]
    status: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let args = Args::parse();
    validate_origin(&args.server)?;
    validate_install_path(&args.agent)?;
    validate_install_path(&args.updater)?;
    validate_service(&args.service)?;
    if let Some(parent) = args.status.parent() {
        fs::create_dir_all(parent).await?;
    }
    let request_id = format!("rupd_{}", uuid::Uuid::new_v4().simple());
    let backup_root = args
        .status
        .parent()
        .unwrap_or(Path::new("/var/lib/joe-mcp-rust-agent"))
        .join("update-backups")
        .join(&request_id);
    fs::create_dir_all(&backup_root).await?;
    let started = chrono::Utc::now().timestamp();
    let previous = installed_version(&args.agent).await;
    write_status(&args.status,json!({"version":1,"request_id":request_id,"state":"running","started_at":started,"finished_at":Value::Null,"previous_version":previous,"installed_version":previous,"rollback_attempted":false,"rollback_succeeded":false,"error":Value::Null})).await?;
    let result = perform_update(&args, &backup_root).await;
    match result {
        Ok(version) => {
            write_status(&args.status,json!({"version":1,"request_id":request_id,"state":"passed","started_at":started,"finished_at":chrono::Utc::now().timestamp(),"previous_version":previous,"installed_version":version,"rollback_attempted":false,"rollback_succeeded":false,"error":Value::Null})).await?;
            prune_backups(
                args.status
                    .parent()
                    .unwrap_or(Path::new("/var/lib/joe-mcp-rust-agent"))
                    .join("update-backups"),
                &backup_root,
            )
            .await;
            Ok(())
        }
        Err(error) => {
            let rollback = rollback(&args, &backup_root).await;
            let installed = installed_version(&args.agent).await;
            write_status(&args.status,json!({"version":1,"request_id":request_id,"state":if rollback{"failed_rolled_back"}else{"failed"},"started_at":started,"finished_at":chrono::Utc::now().timestamp(),"previous_version":previous,"installed_version":installed,"rollback_attempted":true,"rollback_succeeded":rollback,"error":error.to_string().chars().take(500).collect::<String>()})).await?;
            Err(error)
        }
    }
}

async fn perform_update(args: &Args, backup: &Path) -> Result<String> {
    snapshot(&args.agent, &backup.join("agent")).await?;
    snapshot(&args.updater, &backup.join("updater")).await?;
    let client = Client::builder()
        .timeout(Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::limited(2))
        .build()?;
    let agent = download_verified(&client, &args.server, "rust-agent").await?;
    let updater = download_verified(&client, &args.server, "rust-updater").await?;
    replace_executable(&args.agent, &agent).await?;
    replace_executable(&args.updater, &updater).await?;
    systemctl(&["daemon-reload"]).await?;
    systemctl(&["restart", &args.service]).await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    if !systemctl_ok(&["is-active", "--quiet", &args.service]).await {
        bail!("Rust managed agent is not active after update");
    }
    Ok(installed_version(&args.agent).await)
}
async fn rollback(args: &Args, backup: &Path) -> bool {
    let restored = restore_snapshot(&backup.join("agent"), &args.agent)
        .await
        .is_ok()
        && restore_snapshot(&backup.join("updater"), &args.updater)
            .await
            .is_ok();
    if !restored {
        return false;
    }
    let _ = systemctl(&["daemon-reload"]).await;
    let _ = systemctl(&["restart", &args.service]).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    systemctl_ok(&["is-active", "--quiet", &args.service]).await
}
async fn download_verified(client: &Client, server: &str, name: &str) -> Result<Vec<u8>> {
    let origin = server.trim_end_matches('/');
    let payload = client
        .get(format!("{origin}/{name}"))
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?
        .to_vec();
    if payload.is_empty() || payload.len() > 200_000_000 {
        bail!("invalid {name} payload size");
    }
    let checksum = client
        .get(format!("{origin}/{name}.sha256"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let wanted = checksum.split_whitespace().next().unwrap_or("");
    if wanted.len() != 64 || !wanted.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("invalid {name} checksum response");
    }
    let actual = hex::encode(Sha256::digest(&payload));
    if !actual.eq_ignore_ascii_case(wanted) {
        bail!("{name} checksum verification failed");
    }
    Ok(payload)
}
async fn snapshot(source: &Path, dest: &Path) -> Result<()> {
    if source.is_file() {
        fs::copy(source, dest).await?;
    } else {
        fs::write(dest.with_extension("missing"), b"missing").await?;
    }
    Ok(())
}
async fn restore_snapshot(backup: &Path, dest: &Path) -> Result<()> {
    if backup.is_file() {
        replace_executable(dest, &fs::read(backup).await?).await
    } else if backup.with_extension("missing").is_file() {
        let _ = fs::remove_file(dest).await;
        Ok(())
    } else {
        bail!("rollback snapshot is missing")
    }
}
async fn replace_executable(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("install path has no parent"))?;
    fs::create_dir_all(parent).await?;
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("joe-rust")
    ));
    fs::write(&tmp, data).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).await?;
    }
    fs::rename(tmp, path).await?;
    Ok(())
}
async fn installed_version(agent: &Path) -> String {
    match Command::new(agent).arg("--version").output().await {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .trim()
            .chars()
            .take(80)
            .collect(),
        _ => "unknown".into(),
    }
}
async fn systemctl(args: &[&str]) -> Result<()> {
    let out = Command::new("systemctl").args(args).output().await?;
    if !out.status.success() {
        bail!("systemctl {} failed", args.join(" "));
    }
    Ok(())
}
async fn systemctl_ok(args: &[&str]) -> bool {
    Command::new("systemctl")
        .args(args)
        .status()
        .await
        .is_ok_and(|s| s.success())
}
async fn write_status(path: &Path, value: Value) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("status path has no parent"))?;
    fs::create_dir_all(parent).await?;
    let tmp = parent.join(".update-status.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&value)?).await?;
    fs::rename(tmp, path).await?;
    Ok(())
}
async fn prune_backups(root: PathBuf, keep: &Path) {
    let Ok(mut rd) = fs::read_dir(root).await else {
        return;
    };
    let mut dirs = Vec::new();
    while let Ok(Some(e)) = rd.next_entry().await {
        if e.path() != keep && e.file_type().await.is_ok_and(|t| t.is_dir()) {
            dirs.push(e.path());
        }
    }
    dirs.sort();
    if dirs.len() > 1 {
        let remove_count = dirs.len() - 1;
        for p in dirs.into_iter().take(remove_count) {
            let _ = fs::remove_dir_all(p).await;
        }
    }
}
fn validate_origin(raw: &str) -> Result<()> {
    let u = url::Url::parse(raw)?;
    if u.scheme() != "https"
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
        || !matches!(u.path(), "" | "/")
    {
        bail!("server must be a clean HTTPS origin");
    }
    Ok(())
}
fn validate_install_path(p: &Path) -> Result<()> {
    let s = p.to_string_lossy();
    if !p.is_absolute() || s == "/" || !s.starts_with("/usr/local/libexec/joe-mcp-rust-") {
        bail!("unsafe Rust agent install path");
    }
    Ok(())
}
fn validate_service(s: &str) -> Result<()> {
    if s != "joe-mcp-rust-agent.service" {
        bail!("unexpected service name");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn binary_snapshot_restore_recovers_previous_managed_agent_payloads() {
        let root = std::env::temp_dir().join(format!("joe-rust-updater-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).await.unwrap();
        let agent = root.join("joe-mcp-rust-agent");
        let updater = root.join("joe-mcp-rust-updater");
        let backup = root.join("backup");
        fs::create_dir_all(&backup).await.unwrap();

        fs::write(&agent, b"old-agent").await.unwrap();
        fs::write(&updater, b"old-updater").await.unwrap();
        snapshot(&agent, &backup.join("agent")).await.unwrap();
        snapshot(&updater, &backup.join("updater")).await.unwrap();

        fs::write(&agent, b"broken-new-agent").await.unwrap();
        fs::write(&updater, b"broken-new-updater").await.unwrap();
        restore_snapshot(&backup.join("agent"), &agent)
            .await
            .unwrap();
        restore_snapshot(&backup.join("updater"), &updater)
            .await
            .unwrap();

        assert_eq!(fs::read(&agent).await.unwrap(), b"old-agent");
        assert_eq!(fs::read(&updater).await.unwrap(), b"old-updater");
        let _ = fs::remove_dir_all(root).await;
    }

    #[test]
    fn updater_restricts_origin_install_paths_and_service_name() {
        assert!(validate_origin("https://mcp.example.test").is_ok());
        assert!(validate_origin("http://mcp.example.test").is_err());
        assert!(validate_install_path(Path::new("/usr/local/libexec/joe-mcp-rust-agent")).is_ok());
        assert!(validate_install_path(Path::new("/usr/bin/joe-mcp-rust-agent")).is_err());
        assert!(validate_service("joe-mcp-rust-agent.service").is_ok());
        assert!(validate_service("ssh.service").is_err());
    }
}
