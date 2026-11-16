//! Native installation is staging, never a service/container restart.
//! Ownership approval is separate and refuses an active legacy controller.
use crate::{
    authorization::{Failure, RestAuthorizer, Result},
    controller::Config,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};
const BINS: [&str; 4] = [
    "cbgpu-gpu-controller",
    "cbgpu-cpu-controller",
    "cbgpu-ssh-gateway",
    "cbgpu-native-install",
];
const UNITS: [&str; 4] = [
    "cbgpu-gpu-controller.service",
    "cbgpu-cpu-controller.service",
    "cbgpu-ssh-gateway.service",
    "cbgpu-ssh-gateway-lan.service",
];
const TEMPLATES: [&str; 4] = [
    include_str!("../deploy/systemd/cbgpu-gpu-controller.service"),
    include_str!("../deploy/systemd/cbgpu-cpu-controller.service"),
    include_str!("../deploy/systemd/cbgpu-ssh-gateway.service"),
    include_str!("../deploy/systemd/cbgpu-ssh-gateway-lan.service"),
];
#[derive(Clone)]
pub struct Settings {
    pub version: String,
    pub config: PathBuf,
    pub service_key: PathBuf,
    pub ssh_key: PathBuf,
    pub binaries: PathBuf,
    pub legacy_user: String,
}
#[derive(Clone)]
pub struct Layout {
    pub root: PathBuf,
}
impl Layout {
    pub fn path(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceState {
    pub load: String,
    pub active: String,
    pub pid: u32,
    pub enabled: String,
    pub dropins: bool,
}
impl ServiceState {
    pub fn stopped(&self) -> bool {
        self.pid == 0
            && matches!(self.active.as_str(), "inactive" | "failed")
            && matches!(self.load.as_str(), "loaded" | "not-found" | "masked")
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContainerState {
    pub id: String,
    pub running: bool,
    pub restarts: u64,
    pub started_at: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub created: i64,
    pub release: String,
    pub approval: String,
    pub config_sha256: String,
    pub service_key_sha256: String,
    pub ssh_key_sha256: String,
    pub binaries: BTreeMap<String, String>,
    pub units: BTreeMap<String, String>,
    pub containers: BTreeMap<String, ContainerState>,
    pub approved: bool,
}
#[async_trait::async_trait]
pub trait Host: Send + Sync {
    fn is_root(&self) -> bool;
    fn owner(&self) -> u32;
    fn service(&self, unit: &str, user: Option<&str>) -> Result<ServiceState>;
    fn containers(&self, names: &[String]) -> Result<BTreeMap<String, ContainerState>>;
    fn configured_notebook(&self, name: &str) -> Result<bool>;
    fn reload_units(&self) -> Result<()>;
    async fn backend_ready(&self, config: &Config, key: &str) -> Result<()>;
}
pub struct NativeHost;
#[async_trait::async_trait]
impl Host for NativeHost {
    fn is_root(&self) -> bool {
        unsafe { libc::geteuid() == 0 }
    }
    fn owner(&self) -> u32 {
        unsafe { libc::geteuid() }
    }
    fn service(&self, unit: &str, user: Option<&str>) -> Result<ServiceState> {
        let mut command = if let Some(user) = user {
            if user.is_empty()
                || !user
                    .bytes()
                    .all(|v| v.is_ascii_alphanumeric() || b"_-".contains(&v))
            {
                return Err(Failure("Invalid legacy service user"));
            }
            let id = std::process::Command::new("/usr/bin/id")
                .args(["-u", user])
                .output()
                .map_err(|_| Failure("Cannot inspect legacy service user"))?;
            let uid = std::str::from_utf8(&id.stdout)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .filter(|_| id.status.success())
                .ok_or(Failure("Legacy service user unavailable"))?;
            let mut command = std::process::Command::new("/usr/sbin/runuser");
            command.args([
                "-u",
                user,
                "--",
                "/usr/bin/env",
                &format!("XDG_RUNTIME_DIR=/run/user/{uid}"),
                "/usr/bin/systemctl",
                "--user",
            ]);
            command
        } else {
            std::process::Command::new("/usr/bin/systemctl")
        };
        let output = command
            .args([
                "show",
                unit,
                "--property=LoadState,ActiveState,MainPID,UnitFileState,DropInPaths",
            ])
            .output()
            .map_err(|_| Failure("Cannot inspect controller ownership"))?;
        let text =
            std::str::from_utf8(&output.stdout).map_err(|_| Failure("Invalid service state"))?;
        let values = text
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect::<BTreeMap<_, _>>();
        let load = values
            .get("LoadState")
            .ok_or(Failure("Service ownership is unknown"))?
            .to_string();
        let active = values
            .get("ActiveState")
            .ok_or(Failure("Service ownership is unknown"))?
            .to_string();
        let pid = values
            .get("MainPID")
            .and_then(|v| v.parse().ok())
            .ok_or(Failure("Service ownership is unknown"))?;
        if !output.status.success() && !matches!(load.as_str(), "not-found" | "masked") {
            return Err(Failure("Service inspection failed"));
        }
        let enabled = values
            .get("UnitFileState")
            .map(|v| v.to_string())
            .unwrap_or_default();
        Ok(ServiceState {
            load,
            active,
            pid,
            enabled,
            dropins: values.get("DropInPaths").is_some_and(|v| !v.is_empty()),
        })
    }
    fn containers(&self, names: &[String]) -> Result<BTreeMap<String, ContainerState>> {
        let mut values = BTreeMap::new();
        for name in names {
            let output=std::process::Command::new("/usr/bin/docker").args(["inspect","--format",r#"{"id":{{json .Id}},"running":{{json .State.Running}},"restarts":{{.RestartCount}},"started_at":{{json .State.StartedAt}}}"#,name]).output().map_err(|_|Failure("Cannot inspect notebook containers"))?;
            if !output.status.success() {
                return Err(Failure(
                    "Configured notebook is unavailable; no changes made",
                ));
            }
            let state: ContainerState = serde_json::from_slice(&output.stdout)
                .map_err(|_| Failure("Invalid notebook identity snapshot"))?;
            values.insert(name.clone(), state);
        }
        Ok(values)
    }
    fn configured_notebook(&self, name: &str) -> Result<bool> {
        let out = std::process::Command::new("/usr/bin/docker")
            .args([
                "exec",
                name,
                "test",
                "-s",
                "/root/.jupyter/jupyter_server_config.py",
            ])
            .output()
            .map_err(|_| Failure("Cannot inspect existing notebook authentication"))?;
        Ok(out.status.success())
    }
    fn reload_units(&self) -> Result<()> {
        let output = std::process::Command::new("/usr/bin/systemctl")
            .arg("daemon-reload")
            .output()
            .map_err(|_| Failure("Cannot reload unit definitions"))?;
        if !output.status.success() {
            return Err(Failure(
                "Unit reload failed; staged services remain unapproved",
            ));
        }
        Ok(())
    }
    async fn backend_ready(&self, config: &Config, key: &str) -> Result<()> {
        RestAuthorizer::new(&config.supabase_url, key.to_owned())?
            .snapshot()
            .await?;
        Ok(())
    }
}
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn file(path: &Path, private: bool) -> Result<Vec<u8>> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::symlink_metadata(path)
        .map_err(|_| Failure("Required installation input unavailable"))?;
    if !meta.is_file()
        || meta.file_type().is_symlink()
        || (private && meta.permissions().mode() & 0o077 != 0)
        || (!private && meta.permissions().mode() & 0o022 != 0)
    {
        return Err(Failure(
            "Installation inputs must be regular files with safe permissions",
        ));
    }
    std::fs::read(path).map_err(|_| Failure("Cannot read installation input"))
}
fn safe_ancestors(path: &Path) -> Result<()> {
    for parent in path.ancestors() {
        match std::fs::symlink_metadata(parent) {
            Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => {
                return Err(Failure(
                    "Installation directory contains a symlink or special file",
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(Failure("Cannot inspect installation directory")),
        }
    }
    Ok(())
}
fn write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let parent = path
        .parent()
        .ok_or(Failure("Installation destination missing"))?;
    safe_ancestors(parent)?;
    std::fs::create_dir_all(parent).map_err(|_| Failure("Cannot create installation directory"))?;
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && (!meta.is_file() || meta.file_type().is_symlink())
    {
        return Err(Failure("Refusing to replace a symlink or special file"));
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|_| Failure("Cannot stage installation file"))?;
    temp.write_all(bytes)
        .map_err(|_| Failure("Cannot stage installation file"))?;
    temp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(|_| Failure("Cannot secure installation file"))?;
    temp.as_file()
        .sync_all()
        .map_err(|_| Failure("Cannot sync installation file"))?;
    temp.persist(path)
        .map_err(|_| Failure("Cannot activate staged installation file"))?;
    Ok(())
}
fn private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    safe_ancestors(path)?;
    std::fs::create_dir_all(path)
        .map_err(|_| Failure("Cannot create private installation directory"))?;
    if std::fs::metadata(path)
        .map_err(|_| Failure("Cannot inspect private directory owner"))?
        .uid()
        != unsafe { libc::geteuid() }
    {
        return Err(Failure(
            "Private installer directories must belong to the installing identity",
        ));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| Failure("Cannot protect installation directory"))
}
struct Inputs {
    cfg: Config,
    config: Vec<u8>,
    service: Vec<u8>,
    ssh: Vec<u8>,
}
fn config(settings: &Settings) -> Result<Inputs> {
    if settings.version.is_empty()
        || settings.version.len() > 64
        || !settings
            .version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(Failure("Invalid native release version"));
    }
    let bytes = file(&settings.config, true)?;
    let raw: serde_yaml_ng::Value = serde_yaml_ng::from_slice(&bytes)
        .map_err(|_| Failure("Invalid private controller configuration"))?;
    if raw
        .get("auth_method")
        .and_then(|v| v.as_str())
        .is_some_and(|v| v != "pam")
    {
        return Err(Failure(
            "Legacy NativeAuthenticator command adapters require a separately reviewed port; no activation performed",
        ));
    }
    let config: Config = serde_yaml_ng::from_slice(&bytes)
        .map_err(|_| Failure("Invalid controller configuration"))?;
    config.validate()?;
    if config.initialize_idle_containers {
        return Err(Failure(
            "Native installation requires initialize_idle_containers=false to preserve notebooks",
        ));
    }
    let keys = Path::new(&config.docker.ssh_host_keys_dir);
    if !keys.is_absolute()
        || keys.parent().is_none()
        || keys == Path::new("/")
        || keys
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        || config
            .docker
            .ssh_host_keys_dir
            .chars()
            .any(char::is_whitespace)
        || config.docker.ssh_host_keys_dir.contains(['\\', '"'])
    {
        return Err(Failure(
            "SSH host key directory must be a specific absolute private directory",
        ));
    }
    let url =
        reqwest::Url::parse(&config.supabase_url).map_err(|_| Failure("Invalid backend URL"))?;
    if !["http", "https"].contains(&url.scheme())
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
        || config.supabase_url.chars().any(char::is_whitespace)
    {
        return Err(Failure(
            "Backend URL cannot contain credentials or control text",
        ));
    }
    let service = file(&settings.service_key, true)?;
    let key = std::str::from_utf8(&service)
        .map_err(|_| Failure("Invalid service credential"))?
        .trim();
    if key.len() < 20 {
        return Err(Failure("Service credential unavailable"));
    }
    RestAuthorizer::new(&config.supabase_url, key.to_owned())?;
    let ssh = file(&settings.ssh_key, true)?;
    let parsed = russh::keys::load_secret_key(&settings.ssh_key, None)
        .map_err(|_| Failure("Cannot verify existing SSH gateway host key"))?;
    if parsed.algorithm() != russh::keys::Algorithm::Ed25519 {
        return Err(Failure("Existing gateway key must be Ed25519"));
    }
    Ok(Inputs {
        cfg: config,
        config: bytes,
        service,
        ssh,
    })
}
pub fn plan(settings: &Settings) -> serde_json::Value {
    serde_json::json!({"mode":"plan","apply":false,"version":settings.version,"files":"versioned native binaries, private config/credential copies, inactive service definitions and private backups","services_started":false,"containers_changed":false,"handoff":"separate approval requires all legacy duties inactive and explicit CPU-job drain confirmation; no cooperative legacy pause protocol is implemented"})
}
fn exclusive(settings: &Settings, host: &dyn Host) -> Result<()> {
    for (unit, user) in [
        ("cbgpu-daemon.service", None),
        ("cbgpu-gpu-controller.service", None),
        ("cbgpu-cpu-controller.service", None),
        ("cbgpu-ssh-gateway.service", None),
        ("cbgpu-ssh-gateway-lan.service", None),
        (
            "cbgpu-ssh-gateway.service",
            Some(settings.legacy_user.as_str()),
        ),
        (
            "cbgpu-ssh-gateway-lan.service",
            Some(settings.legacy_user.as_str()),
        ),
    ] {
        let state = host.service(unit, user)?;
        if !state.stopped() || (user.is_none() && UNITS.contains(&unit) && state.dropins) {
            return Err(Failure(
                "A controller still owns duties; active/cooperative handoff cannot be proven and was refused",
            ));
        }
        if (unit == "cbgpu-daemon.service" || user.is_some())
            && state.load != "not-found"
            && !matches!(
                state.enabled.as_str(),
                "disabled" | "masked" | "masked-runtime"
            )
        {
            return Err(Failure(
                "Legacy services must be disabled or masked by the operator before approving ownership",
            ));
        }
    }
    Ok(())
}
fn install_lock(layout: &Layout) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let directory = layout.path("/var/lib/cbgpu-native-install");
    private_dir(&directory)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join("install.lock"))
        .map_err(|_| Failure("Cannot open private installer lock"))?;
    fs2::FileExt::try_lock_exclusive(&file)
        .map_err(|_| Failure("Another native installer owns this operation"))?;
    Ok(file)
}
pub async fn stage(settings: &Settings, layout: &Layout, host: &dyn Host) -> Result<Manifest> {
    if !host.is_root() {
        return Err(Failure(
            "Installation requires root; no files or services changed",
        ));
    }
    let Inputs {
        cfg,
        config,
        service,
        ssh,
    } = config(settings)?;
    for unit in UNITS {
        let state = host.service(unit, None)?;
        if !state.stopped() || state.dropins {
            return Err(Failure(
                "A native service is active; do not overwrite its unit during work",
            ));
        }
    }
    let _lock = install_lock(layout)?;
    // Existing legacy services may keep working while an unapproved release is staged.
    let names = cfg
        .gpus
        .values()
        .map(|g| g.container.clone())
        .collect::<Vec<_>>();
    let containers = host.containers(&names)?;
    let mut binaries = BTreeMap::new();
    let mut hashes = BTreeMap::new();
    for name in BINS {
        let bytes = file(&settings.binaries.join(name), false)?;
        if bytes.len() < 64 || &bytes[..4] != b"\x7fELF" {
            return Err(Failure("Native binary must be a compiled ELF executable"));
        }
        hashes.insert(name.to_owned(), digest(&bytes));
        binaries.insert(name.to_owned(), bytes);
    }
    let id = digest(
        &serde_json::to_vec(&(&hashes, digest(&config), digest(&service), digest(&ssh)))
            .map_err(|_| Failure("Cannot identify native release"))?,
    );
    let release = format!("/opt/cbgpu-native/{}-{}", settings.version, &id[..12]);
    let approval = format!("/etc/cbgpu/native-approved-{}.json", &id[..16]);
    if layout.path(&approval).exists() {
        return Err(Failure(
            "This release is already approved; do not restage a live handoff",
        ));
    }
    for (name, bytes) in [("service-role-key", &service), ("ssh-gateway-key", &ssh)] {
        let target = layout.path(&format!("/etc/cbgpu/{name}"));
        if target.exists() && file(&target, true)? != *bytes {
            return Err(Failure(
                "Existing private credential differs and was preserved; review explicitly before rotating",
            ));
        }
    }
    let backup = layout.path(&format!(
        "/var/lib/cbgpu-native-install/backups/{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        rand::random::<u64>()
    ));
    private_dir(&backup)?;
    for path in [
        "/etc/systemd/system/cbgpu-daemon.service",
        "/etc/systemd/system/cbgpu-gpu-controller.service",
        "/etc/systemd/system/cbgpu-cpu-controller.service",
        "/etc/systemd/system/cbgpu-ssh-gateway.service",
        "/etc/systemd/system/cbgpu-ssh-gateway-lan.service",
        "/etc/cbgpu-daemon.env",
        "/etc/cbgpu/native-controller.env",
        "/etc/cbgpu/controller.yaml",
        "/etc/cbgpu/service-role-key",
        "/etc/cbgpu/ssh-gateway-key",
        "/var/lib/cbgpu-native-install/staged.json",
    ] {
        let target = layout.path(path);
        if target.exists() {
            let bytes = file(&target, false)?;
            write(
                &backup.join(path.trim_start_matches('/').replace('/', "__")),
                &bytes,
                0o600,
            )?;
        }
    }
    private_dir(&layout.path("/etc/cbgpu"))?;
    for directory in [
        "/opt/cbgpu-native".to_owned(),
        release.clone(),
        format!("{release}/bin"),
    ] {
        let path = layout.path(&directory);
        safe_ancestors(&path)?;
        let existed = path.exists();
        std::fs::create_dir_all(&path)
            .map_err(|_| Failure("Cannot create immutable binary directory"))?;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if !existed {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .map_err(|_| Failure("Cannot secure binary directory"))?;
        }
        let meta =
            std::fs::metadata(&path).map_err(|_| Failure("Cannot inspect binary directory"))?;
        if meta.uid() != host.owner()
            || meta.permissions().mode() & 0o022 != 0
            || meta.permissions().mode() & 0o001 == 0
        {
            return Err(Failure(
                "Immutable binary directories must be owned by root and not group/world writable",
            ));
        }
    }
    for (name, bytes) in &binaries {
        let path = layout.path(&format!("{release}/bin/{name}"));
        if path.exists() {
            use std::os::unix::fs::MetadataExt;
            if std::fs::symlink_metadata(&path)
                .map_err(|_| Failure("Cannot inspect binary owner"))?
                .uid()
                != host.owner()
            {
                return Err(Failure("Existing binary has an unsafe owner"));
            }
            if std::fs::metadata(&path)
                .map_err(|_| Failure("Cannot inspect binary permissions"))?
                .permissions()
                .mode()
                & 0o111
                != 0o111
            {
                return Err(Failure(
                    "Existing release binary is not executable by service users",
                ));
            }
            if file(&path, false)? != *bytes {
                return Err(Failure(
                    "Versioned binary collision; existing release preserved",
                ));
            }
        } else {
            write(&path, bytes, 0o755)?;
        }
    }
    write(&layout.path("/etc/cbgpu/controller.yaml"), &config, 0o600)?;
    write(&layout.path("/etc/cbgpu/service-role-key"), &service, 0o600)?;
    write(&layout.path("/etc/cbgpu/ssh-gateway-key"), &ssh, 0o600)?;
    let key_directory = layout.path(&cfg.docker.ssh_host_keys_dir);
    if !key_directory.exists() {
        private_dir(&key_directory)?;
    } else {
        safe_ancestors(&key_directory)?;
    }
    let mut unit_hashes = BTreeMap::new();
    for (index, unit) in UNITS.iter().enumerate() {
        let mut text = TEMPLATES[index]
            .replace("/opt/cbgpu-native/0.1.0", &release)
            .replace("/etc/cbgpu/native-controller-approved", &approval);
        if index == 0 {
            text.push_str(&format!(
                "\n[Service]\nReadWritePaths={}\n",
                cfg.docker.ssh_host_keys_dir.replace('%', "%%")
            ));
        }
        if index >= 2 {
            text = text.replace(
                "Environment=CBGPU_SUPABASE_URL=http://127.0.0.1:54321",
                &format!(
                    "Environment=CBGPU_SUPABASE_URL={}",
                    reqwest::Url::parse(&cfg.supabase_url)
                        .map_err(|_| Failure("Invalid backend URL"))?
                        .as_str()
                        .replace('%', "%%")
                ),
            );
        }
        if index == 3 {
            text = text.replace(
                "Environment=CBGPU_GATEWAY_LISTEN_ADDRESS=10.138.23.12:22220",
                &format!(
                    "Environment=CBGPU_GATEWAY_LISTEN_ADDRESS={}:22220",
                    cfg.docker.school_bind_address
                ),
            );
        }
        text.push_str("\n[Service]\nEnvironment=CBGPU_ACCOUNT_V2=0\nEnvironment=CBGPU_CPU_DIAGNOSTICS_DB=false\n");
        unit_hashes.insert(unit.to_string(), digest(text.as_bytes()));
        write(
            &layout.path(&format!("/etc/systemd/system/{unit}")),
            text.as_bytes(),
            0o644,
        )?;
    }
    if host.containers(&names)? != containers {
        return Err(Failure(
            "Notebook identity/running state changed concurrently; staged units remain unapproved",
        ));
    }
    let manifest = Manifest {
        version: settings.version.clone(),
        created: chrono::Utc::now().timestamp(),
        release,
        approval,
        config_sha256: digest(&config),
        service_key_sha256: digest(&service),
        ssh_key_sha256: digest(&ssh),
        binaries: hashes,
        units: unit_hashes,
        containers,
        approved: false,
    };
    private_dir(&layout.path("/var/lib/cbgpu-native-install"))?;
    write(
        &layout.path("/var/lib/cbgpu-native-install/staged.json"),
        &serde_json::to_vec_pretty(&manifest)
            .map_err(|_| Failure("Cannot encode staging manifest"))?,
        0o600,
    )?;
    host.reload_units()?;
    Ok(manifest)
}
pub async fn approve(
    settings: &Settings,
    layout: &Layout,
    host: &dyn Host,
    cpu_jobs_drained: bool,
) -> Result<Manifest> {
    if !host.is_root() {
        return Err(Failure("Ownership approval requires root"));
    }
    if !cpu_jobs_drained {
        return Err(Failure(
            "Explicit CPU-job drain confirmation required; no worker is stopped automatically",
        ));
    }
    exclusive(settings, host)?;
    let _lock = install_lock(layout)?;
    private_dir(&layout.path("/etc/cbgpu"))?;
    let Inputs {
        cfg,
        config,
        service,
        ssh,
    } = config(settings)?;
    let bytes = file(
        &layout.path("/var/lib/cbgpu-native-install/staged.json"),
        true,
    )?;
    let mut manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|_| Failure("Invalid private staging manifest"))?;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    for path in [
        "/var/lib/cbgpu-native-install/staged.json",
        "/etc/cbgpu/controller.yaml",
        "/etc/cbgpu/service-role-key",
        "/etc/cbgpu/ssh-gateway-key",
    ] {
        if std::fs::symlink_metadata(layout.path(path))
            .map_err(|_| Failure("Staged file unavailable"))?
            .uid()
            != host.owner()
        {
            return Err(Failure(
                "Staged files must belong to the installing root identity",
            ));
        }
    }
    if digest(&config) != manifest.config_sha256
        || digest(&service) != manifest.service_key_sha256
        || digest(&ssh) != manifest.ssh_key_sha256
        || manifest.version != settings.version
    {
        return Err(Failure(
            "Reviewed staging inputs changed; restage before approval",
        ));
    }
    for (name, expected) in &manifest.binaries {
        let path = layout.path(&format!("{}/bin/{name}", manifest.release));
        if std::fs::metadata(&path)
            .map_err(|_| Failure("Staged binary permissions unavailable"))?
            .permissions()
            .mode()
            & 0o111
            != 0o111
            || digest(&file(&path, false)?) != *expected
            || std::fs::symlink_metadata(&path)
                .map_err(|_| Failure("Staged binary owner unavailable"))?
                .uid()
                != host.owner()
        {
            return Err(Failure("Staged binary changed; approval refused"));
        }
    }
    for (path, expected) in [
        ("/etc/cbgpu/controller.yaml", &manifest.config_sha256),
        ("/etc/cbgpu/service-role-key", &manifest.service_key_sha256),
        ("/etc/cbgpu/ssh-gateway-key", &manifest.ssh_key_sha256),
    ] {
        if digest(&file(&layout.path(path), true)?) != *expected {
            return Err(Failure(
                "Installed configuration or credentials changed; approval refused",
            ));
        }
    }
    let id = digest(
        &serde_json::to_vec(&(
            &manifest.binaries,
            &manifest.config_sha256,
            &manifest.service_key_sha256,
            &manifest.ssh_key_sha256,
        ))
        .map_err(|_| Failure("Cannot verify release manifest"))?,
    );
    if manifest.binaries.len() != BINS.len()
        || !BINS.iter().all(|key| manifest.binaries.contains_key(*key))
        || manifest.release != format!("/opt/cbgpu-native/{}-{}", settings.version, &id[..12])
        || manifest.approval != format!("/etc/cbgpu/native-approved-{}.json", &id[..16])
    {
        return Err(Failure(
            "Release manifest paths or binary set do not match reviewed hashes",
        ));
    }

    if manifest.units.len() != UNITS.len() || !UNITS.iter().all(|u| manifest.units.contains_key(*u))
    {
        return Err(Failure("Staged service unit set is incomplete"));
    }
    for (unit, expected) in &manifest.units {
        let path = layout.path(&format!("/etc/systemd/system/{unit}"));
        if digest(&file(&path, false)?) != *expected
            || std::fs::symlink_metadata(&path)
                .map_err(|_| Failure("Staged unit unavailable"))?
                .uid()
                != host.owner()
        {
            return Err(Failure("Staged unit changed; approval refused"));
        }
    }
    let names = cfg
        .gpus
        .values()
        .map(|g| g.container.clone())
        .collect::<Vec<_>>();
    let before = host.containers(&names)?;
    if before != manifest.containers {
        return Err(Failure(
            "Notebook identity changed since review; restage without touching containers",
        ));
    }
    for (name, state) in &before {
        if state.running && !host.configured_notebook(name)? {
            return Err(Failure(
                "Notebook authentication is not configured; refuse a handoff that could restart active work",
            ));
        }
    }
    host.backend_ready(
        &cfg,
        std::str::from_utf8(&service)
            .map_err(|_| Failure("Invalid private service credential"))?
            .trim(),
    )
    .await?;
    if host.containers(&names)? != before {
        return Err(Failure(
            "Notebook state changed during approval; no marker created",
        ));
    }
    exclusive(settings, host)?;
    manifest.approved = true;
    let marker = layout.path(&manifest.approval);
    write(
        &marker,
        &serde_json::to_vec_pretty(&manifest)
            .map_err(|_| Failure("Cannot encode ownership approval"))?,
        0o600,
    )?;
    Ok(manifest)
}
