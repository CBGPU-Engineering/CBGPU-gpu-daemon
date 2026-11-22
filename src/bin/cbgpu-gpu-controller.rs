use cbgpu_gpu_daemon::{
    authorization::{Failure, RestAuthorizer, Result},
    controller::{Config, Driver, GpuConfig, State},
    docker::{Docker, NativeRunner, Runner},
};
use chrono::Utc;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
struct PlanOnly(Docker);
#[async_trait::async_trait]
impl Driver for PlanOnly {
    async fn has_password_config(&self, gpu: &GpuConfig) -> Result<bool> {
        self.0.has_password_config(gpu).await
    }
    async fn reset(&self, _: u8, _: &GpuConfig) -> Result<()> {
        Ok(())
    }
    async fn write_keys(&self, _: &GpuConfig, _: &[String]) -> Result<()> {
        Ok(())
    }
    async fn password(&self, _: &GpuConfig, _: &str) -> Result<()> {
        Ok(())
    }
}
fn private_file(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| Failure("Private state or credential unavailable"))?;
    if !metadata.is_file() {
        return Err(Failure("Invalid private state or credential"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Failure(
                "State and credential files must have private permissions",
            ));
        }
    }
    Ok(())
}
fn save(path: &Path, state: &State) -> Result<()> {
    let parent = path.parent().ok_or(Failure("State directory required"))?;
    std::fs::create_dir_all(parent).map_err(|_| Failure("State directory unavailable"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| Failure("State directory permissions failed"))?;
    }
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .map_err(|_| Failure("Private state staging unavailable"))?;
    use std::io::Write;
    file.write_all(&serde_json::to_vec(state).map_err(|_| Failure("State serialization failed"))?)
        .map_err(|_| Failure("State write failed"))?;
    file.as_file()
        .sync_all()
        .map_err(|_| Failure("State sync failed"))?;
    file.persist(path)
        .map_err(|_| Failure("State activation failed"))?;
    Ok(())
}
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1)
    }
}
async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help") {
        println!(
            "Usage: cbgpu-gpu-controller --check CONFIG | --plan-once CONFIG | --run --apply CONFIG PRIVATE_STATE_JSON\nGPU-only native candidate. Non-Compose CPU deployment requires a separately validated native worker. Existing services are never changed by this command."
        );
        return Ok(());
    }
    let apply = args.iter().any(|a| a == "--apply");
    let mode = args.first().map(String::as_str).unwrap_or("");
    if !["--check", "--plan-once", "--run"].contains(&mode) || (mode == "--run" && !apply) {
        return Err(Failure(
            "Use --run --apply only after reviewing the controller and completing worker cutover",
        ));
    }
    let paths: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let config_path = paths
        .first()
        .ok_or(Failure("Controller config path required"))?;
    let config: Config = serde_yaml_ng::from_str(
        &std::fs::read_to_string(config_path)
            .map_err(|_| Failure("Controller configuration unavailable"))?,
    )
    .map_err(|_| Failure("Invalid controller configuration"))?;
    config.validate()?;
    if mode == "--check" {
        println!(
            "GPU controller configuration validated; no backend or Docker operations performed"
        );
        return Ok(());
    }
    let secret_path = std::env::var_os("CBGPU_SUPABASE_SERVICE_ROLE_KEY_FILE")
        .or_else(|| std::env::var_os("CBGPU_SERVICE_ROLE_KEY_FILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/cbgpu/.config/cbgpu/service_role_key"));
    let secret = if let Ok(value) = std::env::var("CBGPU_SUPABASE_SERVICE_ROLE_KEY") {
        value
    } else {
        private_file(&secret_path)?;
        std::fs::read_to_string(secret_path)
            .map_err(|_| Failure("Service credential unavailable"))?
    };
    let backend = RestAuthorizer::new(&config.supabase_url, secret)?;
    let runner: Arc<dyn Runner> = Arc::new(NativeRunner);
    let vpn = runner
        .run("tailscale", &["ip".into(), "-4".into()], None)
        .await
        .ok()
        .filter(|o| o.success)
        .and_then(|o| o.stdout.lines().next().and_then(|line| line.parse().ok()))
        .unwrap_or(std::net::Ipv4Addr::LOCALHOST);
    let docker = Docker::new(config.clone(), vpn, runner)?;
    if mode == "--plan-once" {
        let snapshot = backend.snapshot().await.ok();
        let report = State::default()
            .reconcile(&config, snapshot, Utc::now(), &PlanOnly(docker))
            .await;
        println!(
            "{}",
            serde_json::json!({"mode":"plan-only","report":report,"mutations_executed":false})
        );
        return Ok(());
    }
    let state_path = PathBuf::from(
        paths
            .get(1)
            .ok_or(Failure("Private controller state path required"))?,
    );
    let parent = state_path
        .parent()
        .ok_or(Failure("Private state directory required"))?;
    std::fs::create_dir_all(parent).map_err(|_| Failure("State directory unavailable"))?;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let metadata =
        std::fs::symlink_metadata(parent).map_err(|_| Failure("State directory unavailable"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Failure("State directory must be a real private directory"));
    }
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| Failure("Cannot secure state directory"))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(state_path.with_extension("lock"))
        .map_err(|_| Failure("Controller lock unavailable"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .map_err(|_| Failure("Another native GPU controller owns this state"))?;
    let mut state = if state_path.exists() {
        private_file(&state_path)?;
        serde_json::from_slice::<State>(
            &std::fs::read(&state_path).map_err(|_| Failure("Controller state unavailable"))?,
        )
        .map_err(|_| Failure("Controller state could not be verified"))?
    } else {
        State::default()
    };
    loop {
        let wait = state
            .active
            .values()
            .map(|session| (session.deadline - Utc::now()).to_std().unwrap_or_default())
            .min()
            .unwrap_or(Duration::from_secs(22))
            .min(Duration::from_secs(22));
        let snapshot = tokio::time::timeout(wait.max(Duration::from_millis(1)), backend.snapshot())
            .await
            .ok()
            .and_then(std::result::Result::ok);
        let report = state
            .reconcile(&config, snapshot, Utc::now(), &docker)
            .await;
        save(&state_path, &state)?;
        println!(
            "{}",
            serde_json::to_string(&report)
                .map_err(|_| Failure("Status report serialization failed"))?
        );
        // Learn extensions before the last verified deadline, without adding
        // runtime grace if the bounded authoritative snapshot fails.
        let delay = state
            .active
            .values()
            .map(|session| (session.deadline - Utc::now()).to_std().unwrap_or_default() / 2)
            .min()
            .unwrap_or(Duration::from_secs(config.poll_interval_seconds))
            .min(Duration::from_secs(config.poll_interval_seconds))
            .max(Duration::from_millis(1));
        tokio::select! {_=tokio::time::sleep(delay)=>{},_=tokio::signal::ctrl_c()=>{save(&state_path,&state)?;return Ok(())}}
    }
}
