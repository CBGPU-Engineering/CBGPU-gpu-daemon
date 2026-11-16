use cbgpu_gpu_daemon::{
    authorization::{Failure, Result},
    controller::Config,
    installer::{self, ContainerState, Host, Layout, ServiceState, Settings},
};
use std::{
    collections::BTreeMap,
    os::unix::fs::PermissionsExt,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
struct FakeHost {
    root: bool,
    states: Mutex<BTreeMap<String, ServiceState>>,
    calls: Mutex<Vec<String>>,
    snapshots: AtomicUsize,
    change_at: usize,
    backend: bool,
    configured: bool,
    resume: AtomicBool,
}
fn stopped() -> ServiceState {
    ServiceState {
        load: "loaded".into(),
        active: "inactive".into(),
        pid: 0,
        enabled: "disabled".into(),
        dropins: false,
    }
}
fn active() -> ServiceState {
    ServiceState {
        load: "loaded".into(),
        active: "active".into(),
        pid: 123,
        enabled: "enabled".into(),
        dropins: false,
    }
}
impl Default for FakeHost {
    fn default() -> Self {
        Self {
            root: true,
            states: Mutex::new(BTreeMap::new()),
            calls: Mutex::new(vec![]),
            snapshots: AtomicUsize::new(0),
            change_at: usize::MAX,
            backend: true,
            configured: true,
            resume: AtomicBool::new(false),
        }
    }
}
#[async_trait::async_trait]
impl Host for FakeHost {
    fn is_root(&self) -> bool {
        self.root
    }
    fn owner(&self) -> u32 {
        unsafe { libc::geteuid() }
    }
    fn service(&self, unit: &str, user: Option<&str>) -> Result<ServiceState> {
        let key = format!("{}:{unit}", user.unwrap_or("system"));
        self.calls.lock().unwrap().push(format!("show:{key}"));
        Ok(self
            .states
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .unwrap_or_else(stopped))
    }
    fn containers(&self, names: &[String]) -> Result<BTreeMap<String, ContainerState>> {
        let n = self.snapshots.fetch_add(1, Ordering::SeqCst);
        self.calls
            .lock()
            .unwrap()
            .push("docker:inspect-only".into());
        Ok(names
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    ContainerState {
                        id: if n >= self.change_at {
                            "changed".into()
                        } else {
                            "preserved-container-id".into()
                        },
                        running: true,
                        restarts: 0,
                        started_at: "preserved-start-time".into(),
                    },
                )
            })
            .collect())
    }
    fn configured_notebook(&self, _: &str) -> Result<bool> {
        self.calls
            .lock()
            .unwrap()
            .push("docker:read-auth-presence-only".into());
        Ok(self.configured)
    }
    fn reload_units(&self) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push("systemctl:daemon-reload".into());
        Ok(())
    }
    async fn backend_ready(&self, _: &Config, _: &str) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push("backend:readonly-snapshot".into());
        if self.resume.load(Ordering::SeqCst) {
            self.states
                .lock()
                .unwrap()
                .insert("system:cbgpu-daemon.service".into(), active());
        }
        if self.backend {
            Ok(())
        } else {
            Err(Failure("Fixture backend unavailable"))
        }
    }
}
struct Fixture {
    _tmp: tempfile::TempDir,
    settings: Settings,
    layout: Layout,
}
impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("inputs");
        std::fs::create_dir(&input).unwrap();
        let bin = input.join("bin");
        std::fs::create_dir(&bin).unwrap();
        for name in [
            "cbgpu-gpu-controller",
            "cbgpu-cpu-controller",
            "cbgpu-ssh-gateway",
            "cbgpu-native-install",
        ] {
            let mut bytes = b"\x7fELF".to_vec();
            bytes.extend(vec![0; 64]);
            bytes.extend(name.as_bytes());
            std::fs::write(bin.join(name), bytes).unwrap();
            std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let config = input.join("controller.yaml");
        std::fs::write(&config,"supabase_url: http://127.0.0.1:1\nsupabase_anon_key: fixture\ninitialize_idle_containers: false\ngpus:\n  1:\n    username: gpu1\n    container: fixture_gpu1\n    device: '0'\n    host_port: 8001\n    ssh_port: 2201\ndocker:\n  image: fixture/gpu:latest\n  container_port: 8888\n  school_bind_address: 10.138.23.12\n  ssh_host_keys_dir: /var/lib/cbgpu-gpu-controller/host-keys\n").unwrap();
        let service = input.join("service.key");
        std::fs::write(&service, "fixture-private-service-key-not-a-real-token").unwrap();
        let ssh = input.join("ssh.key");
        let key: russh::keys::PrivateKey =
            russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[8; 32]).into();
        std::fs::write(
            &ssh,
            key.to_openssh(russh::keys::ssh_key::LineEnding::LF)
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        for file in [&config, &service, &ssh] {
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let layout = Layout {
            root: tmp.path().join("host"),
        };
        let settings = Settings {
            version: "fixture-1".into(),
            config,
            service_key: service,
            ssh_key: ssh,
            binaries: bin,
            legacy_user: "fixture-user".into(),
        };
        Self {
            _tmp: tmp,
            settings,
            layout,
        }
    }
    fn existing(&self, path: &str, bytes: &[u8], mode: u32) {
        let path = self.layout.path(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
}
#[tokio::test]
async fn stage_preserves_notebooks_keys_and_legacy_environment_with_no_activation() {
    let f = Fixture::new();
    f.existing(
        "/etc/cbgpu-daemon.env",
        b"ORIGINAL_PRIVATE_ENV=fixture",
        0o600,
    );
    f.existing(
        "/etc/systemd/system/cbgpu-daemon.service",
        b"original immutable legacy service",
        0o644,
    );
    let host = FakeHost::default();
    host.states
        .lock()
        .unwrap()
        .insert("system:cbgpu-daemon.service".into(), active());
    let result = installer::stage(&f.settings, &f.layout, &host)
        .await
        .unwrap();
    assert!(!result.approved);
    assert!(!f.layout.path(&result.approval).exists());
    assert_eq!(
        std::fs::read(f.layout.path("/etc/cbgpu-daemon.env")).unwrap(),
        b"ORIGINAL_PRIVATE_ENV=fixture"
    );
    assert_eq!(
        std::fs::read(f.layout.path("/etc/cbgpu/ssh-gateway-key")).unwrap(),
        std::fs::read(&f.settings.ssh_key).unwrap()
    );
    assert_eq!(
        std::fs::metadata(f.layout.path("/etc/cbgpu/service-role-key"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let unit = std::fs::read_to_string(
        f.layout
            .path("/etc/systemd/system/cbgpu-gpu-controller.service"),
    )
    .unwrap();
    assert!(unit.contains(&result.release));
    assert!(unit.contains(&result.approval));
    assert!(!unit.contains("EnvironmentFile="));
    assert!(!unit.contains(".py"));
    let backups = f.layout.path("/var/lib/cbgpu-native-install/backups");
    let backup = std::fs::read_dir(backups)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        std::fs::read(backup.join("etc__cbgpu-daemon.env")).unwrap(),
        b"ORIGINAL_PRIVATE_ENV=fixture"
    );
    assert_eq!(
        std::fs::metadata(backup).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(
        installer::approve(&f.settings, &f.layout, &host, true)
            .await
            .is_err()
    );
    assert!(!f.layout.path(&result.approval).exists());
    assert!(
        host.calls
            .lock()
            .unwrap()
            .iter()
            .all(|v| !v.contains("restart")
                && !v.contains("start:")
                && !v.contains("stop:")
                && !v.contains("enable"))
    );
}
#[tokio::test]
async fn inactive_disabled_handoff_checks_fresh_backend_and_only_writes_a_private_receipt() {
    let f = Fixture::new();
    let host = FakeHost::default();
    let stage = installer::stage(&f.settings, &f.layout, &host)
        .await
        .unwrap();
    assert!(
        installer::approve(&f.settings, &f.layout, &host, false)
            .await
            .is_err()
    );
    let result = installer::approve(&f.settings, &f.layout, &host, true)
        .await
        .unwrap();
    assert!(result.approved);
    assert_eq!(result.containers, stage.containers);
    assert_eq!(
        std::fs::metadata(f.layout.path(&result.approval))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(
        host.calls
            .lock()
            .unwrap()
            .contains(&"backend:readonly-snapshot".into())
    );
}
#[tokio::test]
async fn root_and_active_native_checks_precede_installed_file_writes() {
    let f = Fixture::new();
    let host = FakeHost {
        root: false,
        ..Default::default()
    };
    assert!(
        installer::stage(&f.settings, &f.layout, &host)
            .await
            .is_err()
    );
    assert!(!f.layout.root.exists());
    let host = FakeHost::default();
    host.states
        .lock()
        .unwrap()
        .insert("system:cbgpu-gpu-controller.service".into(), active());
    assert!(
        installer::stage(&f.settings, &f.layout, &host)
            .await
            .is_err()
    );
    assert!(!f.layout.root.exists());
}
#[tokio::test]
async fn unsafe_input_permissions_idle_reset_and_symlink_targets_fail_closed() {
    let f = Fixture::new();
    let host = FakeHost::default();
    std::fs::set_permissions(
        &f.settings.service_key,
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(
        installer::stage(&f.settings, &f.layout, &host)
            .await
            .is_err()
    );
    std::fs::set_permissions(
        &f.settings.service_key,
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let original = std::fs::read_to_string(&f.settings.config).unwrap();
    std::fs::write(
        &f.settings.config,
        original.replace(
            "initialize_idle_containers: false",
            "initialize_idle_containers: true",
        ),
    )
    .unwrap();
    assert!(
        installer::stage(&f.settings, &f.layout, &host)
            .await
            .is_err()
    );
    std::fs::write(&f.settings.config, original).unwrap();
    let victim = f.settings.binaries.join("victim");
    std::fs::write(&victim, "untouched").unwrap();
    std::fs::create_dir_all(f.layout.path("/etc/cbgpu")).unwrap();
    std::os::unix::fs::symlink(&victim, f.layout.path("/etc/cbgpu/controller.yaml")).unwrap();
    assert!(
        installer::stage(&f.settings, &f.layout, &host)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(victim).unwrap(), b"untouched");
}
#[tokio::test]
async fn credentials_and_versioned_binaries_cannot_be_silently_rotated() {
    let f = Fixture::new();
    let host = FakeHost::default();
    let stage = installer::stage(&f.settings, &f.layout, &host)
        .await
        .unwrap();
    f.existing("/etc/cbgpu/service-role-key", b"changed-private-key", 0o600);
    assert!(
        installer::approve(&f.settings, &f.layout, &host, true)
            .await
            .is_err()
    );
    assert!(
        installer::stage(&f.settings, &f.layout, &host)
            .await
            .is_err()
    );
    assert!(!f.layout.path(&stage.approval).exists());
}
#[tokio::test]
async fn backend_failure_unconfigured_auth_and_service_resume_never_approve() {
    for mode in 0..3 {
        let f = Fixture::new();
        let host = FakeHost {
            backend: mode != 0,
            configured: mode != 1,
            ..Default::default()
        };
        let stage = installer::stage(&f.settings, &f.layout, &host)
            .await
            .unwrap();
        host.resume.store(mode == 2, Ordering::SeqCst);
        assert!(
            installer::approve(&f.settings, &f.layout, &host, true)
                .await
                .is_err()
        );
        assert!(!f.layout.path(&stage.approval).exists());
    }
}
#[tokio::test]
async fn enabled_legacy_user_gateways_and_mutated_units_block_approval() {
    let f = Fixture::new();
    let host = FakeHost::default();
    let stage = installer::stage(&f.settings, &f.layout, &host)
        .await
        .unwrap();
    let mut legacy = stopped();
    legacy.enabled = "enabled".into();
    host.states
        .lock()
        .unwrap()
        .insert("fixture-user:cbgpu-ssh-gateway.service".into(), legacy);
    assert!(
        installer::approve(&f.settings, &f.layout, &host, true)
            .await
            .is_err()
    );
    host.states.lock().unwrap().clear();
    f.existing(
        "/etc/systemd/system/cbgpu-gpu-controller.service",
        b"changed unit",
        0o644,
    );
    assert!(
        installer::approve(&f.settings, &f.layout, &host, true)
            .await
            .is_err()
    );
    assert!(!f.layout.path(&stage.approval).exists());
}
#[tokio::test]
async fn changing_container_identity_during_staging_prevents_any_approval() {
    let f = Fixture::new();
    let host = FakeHost {
        change_at: 1,
        ..Default::default()
    };
    assert!(
        installer::stage(&f.settings, &f.layout, &host)
            .await
            .is_err()
    );
    assert!(
        !f.layout
            .path("/var/lib/cbgpu-native-install/staged.json")
            .exists()
    );
}
#[test]
fn cli_plan_never_looks_up_services_or_creates_files() {
    let f = Fixture::new();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_cbgpu-native-install"))
        .arg("stage")
        .env("PATH", "/nonexistent")
        .output()
        .unwrap();
    assert!(out.status.success());
    let plan: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(plan["apply"], false);
    assert!(!f.layout.root.exists());
}

#[test]
fn masked_legacy_state_is_stopped_but_unknown_or_running_state_is_not() {
    let mut state = stopped();
    state.load = "masked".into();
    state.enabled = "masked".into();
    assert!(state.stopped());
    state.pid = 42;
    assert!(!state.stopped());
    state.pid = 0;
    state.load = "error".into();
    assert!(!state.stopped());
}
