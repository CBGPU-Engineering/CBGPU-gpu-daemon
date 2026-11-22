use cbgpu_gpu_daemon::{
    authorization::{Failure, Reservation, Result},
    controller::{Config, DockerConfig, Driver, GpuConfig, Snapshot, State},
    docker::{Docker, Output, Runner},
};
use chrono::{Duration, Utc};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
fn config(directory: &std::path::Path) -> Config {
    Config {
        supabase_url: "http://127.0.0.1:54321".into(),
        supabase_anon_key: "synthetic-anon".into(),
        gpus: BTreeMap::from([(
            1,
            GpuConfig {
                username: "gpu1".into(),
                container: "jupiterhub_gpu1".into(),
                device: "0".into(),
                host_port: 8001,
                ssh_port: Some(2201),
            },
        )]),
        docker: DockerConfig {
            image: "cbgpu/gpu-ssh:latest".into(),
            tunnel_image: "cloudflare/cloudflared:latest".into(),
            container_port: 8888,
            school_bind_address: "10.138.23.12".into(),
            ssh_host_keys_dir: directory.to_string_lossy().into_owned(),
        },
        poll_interval_seconds: 30,
        initialize_idle_containers: false,
    }
}
fn booking(id: &str, start: chrono::DateTime<Utc>, end: chrono::DateTime<Utc>) -> Reservation {
    Reservation {
        id: id.into(),
        user_id: "owner-a".into(),
        gpu_id: 1,
        start_time: start,
        end_time: end,
        status: "approved".into(),
    }
}
#[derive(Default)]
struct MockDriver {
    calls: Mutex<Vec<String>>,
    configured: bool,
    reset_fail: bool,
    inspect_fail: bool,
}
#[async_trait::async_trait]
impl Driver for MockDriver {
    async fn has_password_config(&self, _: &GpuConfig) -> Result<bool> {
        if self.inspect_fail {
            Err(Failure("synthetic inspection failure"))
        } else {
            Ok(self.configured)
        }
    }
    async fn reset(&self, id: u8, _: &GpuConfig) -> Result<()> {
        self.calls.lock().unwrap().push(format!("reset{id}"));
        if self.reset_fail {
            Err(Failure("synthetic reset failure"))
        } else {
            Ok(())
        }
    }
    async fn write_keys(&self, _: &GpuConfig, keys: &[String]) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("keys{}", keys.len()));
        Ok(())
    }
    async fn password(&self, _: &GpuConfig, _: &str) -> Result<()> {
        self.calls.lock().unwrap().push("password".into());
        Ok(())
    }
}
#[tokio::test]
async fn restarting_controller_preserves_running_bookings_and_idle_notebooks() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    let at = Utc::now();
    let driver = MockDriver {
        configured: true,
        ..Default::default()
    };
    let mut state = State::default();
    let snapshot = Snapshot {
        passwords: BTreeMap::from([(1, "DO_NOT_LEAK_PASSWORD".into())]),
        reservations: vec![
            booking(
                "active",
                at - Duration::minutes(1),
                at + Duration::minutes(1),
            ),
            booking(
                "historical",
                at - Duration::hours(2),
                at - Duration::hours(1),
            ),
        ],
        keys: Some(BTreeMap::new()),
    };
    let report = state
        .reconcile(&cfg, Some(snapshot.clone()), at, &driver)
        .await;
    assert!(
        !driver
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "password" || call.starts_with("reset"))
    );
    assert!(
        !serde_json::to_string(&report)
            .unwrap()
            .contains("DO_NOT_LEAK_PASSWORD")
    );
    assert!(
        !serde_json::to_string(&state)
            .unwrap()
            .contains("DO_NOT_LEAK_PASSWORD")
    );
    assert_eq!(state.active.len(), 1);
    let _ = state.reconcile(&cfg, Some(snapshot), at, &driver).await;
    assert!(
        !driver
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "password" || call.starts_with("reset"))
    );
    let idle = Snapshot {
        passwords: BTreeMap::new(),
        reservations: vec![],
        keys: None,
    };
    let fresh = MockDriver::default();
    State::default()
        .reconcile(&cfg, Some(idle), at, &fresh)
        .await;
    assert!(fresh.calls.lock().unwrap().is_empty());
}
#[tokio::test]
async fn known_deadline_is_enforced_during_backend_outage_and_failed_reset_is_retried() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    let at = Utc::now();
    let mut state = State::default();
    let driver = MockDriver {
        configured: true,
        ..Default::default()
    };
    state
        .reconcile(
            &cfg,
            Some(Snapshot {
                passwords: BTreeMap::new(),
                reservations: vec![booking(
                    "active",
                    at - Duration::minutes(1),
                    at + Duration::seconds(10),
                )],
                keys: None,
            }),
            at,
            &driver,
        )
        .await;
    let early = state
        .reconcile(&cfg, None, at + Duration::seconds(9), &driver)
        .await;
    assert!(early.actions.is_empty());
    let failing = MockDriver {
        reset_fail: true,
        ..Default::default()
    };
    state
        .reconcile(&cfg, None, at + Duration::seconds(11), &failing)
        .await;
    assert_eq!(state.active.len(), 1);
    let expired = state
        .reconcile(&cfg, None, at + Duration::seconds(12), &driver)
        .await;
    assert_eq!(expired.actions, vec!["gpu1:reset"]);
    assert!(state.active.is_empty());
    assert!(state.reset_done.contains("active"));
}
#[tokio::test]
async fn cancellation_closes_prior_session_before_granting_another_owner_access() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    let at = Utc::now();
    let driver = MockDriver {
        configured: true,
        ..Default::default()
    };
    let mut state = State::default();
    let snapshot = |id: &str| Snapshot {
        passwords: BTreeMap::from([(1, "password".into())]),
        reservations: vec![booking(
            id,
            at - Duration::minutes(1),
            at + Duration::minutes(1),
        )],
        keys: None,
    };
    state
        .reconcile(&cfg, Some(snapshot("first")), at, &driver)
        .await;
    driver.calls.lock().unwrap().clear();
    state
        .reconcile(&cfg, Some(snapshot("second")), at, &driver)
        .await;
    let calls = driver.calls.lock().unwrap();
    assert_eq!(calls[0], "reset1");
    assert!(calls.iter().position(|c| c == "reset1") < calls.iter().position(|c| c == "password"));
}
#[tokio::test]
async fn password_changes_apply_once_and_failed_session_reset_never_grants_new_access() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    let at = Utc::now();
    let mut state = State::default();
    let driver = MockDriver::default();
    let mut snapshot = Snapshot {
        passwords: BTreeMap::from([(1, "initial".into())]),
        reservations: vec![booking(
            "first",
            at - Duration::minutes(1),
            at + Duration::minutes(1),
        )],
        keys: None,
    };
    state
        .reconcile(&cfg, Some(snapshot.clone()), at, &driver)
        .await;
    state
        .reconcile(&cfg, Some(snapshot.clone()), at, &driver)
        .await;
    assert_eq!(
        driver
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.as_str() == "password")
            .count(),
        1
    );
    snapshot.passwords.insert(1, "changed".into());
    state
        .reconcile(&cfg, Some(snapshot.clone()), at, &driver)
        .await;
    assert_eq!(
        driver
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.as_str() == "password")
            .count(),
        2
    );
    snapshot.reservations[0].id = "second".into();
    let failing = MockDriver {
        reset_fail: true,
        ..Default::default()
    };
    state.reconcile(&cfg, Some(snapshot), at, &failing).await;
    assert_eq!(*failing.calls.lock().unwrap(), vec!["reset1"]);
    assert!(state.active[&1].ids.contains("first"));
}
#[tokio::test]
async fn denied_bookings_and_conflicting_owners_do_not_provision_passwords() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    let at = Utc::now();
    let mut pending = booking(
        "pending",
        at - Duration::minutes(1),
        at + Duration::minutes(1),
    );
    pending.status = "pending".into();
    let driver = MockDriver::default();
    State::default()
        .reconcile(
            &cfg,
            Some(Snapshot {
                passwords: BTreeMap::from([(1, "unused".into())]),
                reservations: vec![pending],
                keys: None,
            }),
            at,
            &driver,
        )
        .await;
    assert!(driver.calls.lock().unwrap().is_empty());
    let first = booking("a", at - Duration::minutes(1), at + Duration::minutes(1));
    let mut second = first.clone();
    second.id = "b".into();
    second.user_id = "other-owner".into();
    State::default()
        .reconcile(
            &cfg,
            Some(Snapshot {
                passwords: BTreeMap::from([(1, "unused".into())]),
                reservations: vec![first, second],
                keys: None,
            }),
            at,
            &driver,
        )
        .await;
    assert!(!driver.calls.lock().unwrap().contains(&"password".into()));
}
struct MockRunner {
    calls: Mutex<Vec<Vec<String>>>,
    fail_create: bool,
    fail_start_once: Mutex<bool>,
}
#[async_trait::async_trait]
impl Runner for MockRunner {
    async fn run(&self, program: &str, args: &[String], _: Option<Vec<u8>>) -> Result<Output> {
        let mut call = vec![program.into()];
        call.extend_from_slice(args);
        self.calls.lock().unwrap().push(call);
        if self.fail_create && args.first().is_some_and(|s| s == "create") {
            return Ok(Output {
                success: false,
                ..Default::default()
            });
        }
        if args.first().is_some_and(|s| s == "start") {
            let mut failure = self.fail_start_once.lock().unwrap();
            if *failure {
                *failure = false;
                return Ok(Output {
                    success: false,
                    ..Default::default()
                });
            }
        }
        Ok(Output {
            success: true,
            stdout: "true".into(),
            stderr: String::new(),
        })
    }
}
fn docker(dir: &std::path::Path, runner: Arc<MockRunner>) -> Docker {
    let key = dir.join("1");
    std::fs::create_dir(&key).unwrap();
    std::fs::write(key.join("ssh_host_ed25519_key"), "synthetic key").unwrap();
    let mut docker = Docker::new(config(dir), "100.100.136.37".parse().unwrap(), runner).unwrap();
    docker.verification_delay = std::time::Duration::ZERO;
    docker
}
#[tokio::test]
async fn reset_stages_first_binds_explicit_interfaces_and_preserves_previous_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(MockRunner {
        calls: Mutex::new(vec![]),
        fail_create: false,
        fail_start_once: Mutex::new(false),
    });
    let docker = docker(dir.path(), runner.clone());
    docker.reset(1, &docker.config.gpus[&1]).await.unwrap();
    let calls = runner.calls.lock().unwrap();
    let create = calls
        .iter()
        .find(|c| c.get(1).is_some_and(|v| v == "create"))
        .unwrap();
    for expected in [
        "127.0.0.1:8001:8888",
        "10.138.23.12:8001:8888",
        "100.100.136.37:8001:8888",
        "100.100.136.37:2201:22",
    ] {
        assert!(create.iter().any(|v| v == expected))
    }
    let raw = create.join(" ");
    assert!(!raw.contains("--privileged"));
    assert!(!raw.contains("docker.sock"));
    assert!(raw.contains("/etc/ssh/hostkeys:ro"));
    assert!(
        calls
            .iter()
            .position(|c| c.get(1).is_some_and(|v| v == "create"))
            < calls
                .iter()
                .position(|c| c.get(1).is_some_and(|v| v == "stop"))
    );
    assert!(!calls.iter().any(|c| c.get(1).is_some_and(|v| v == "rm")));
    assert!(
        !calls
            .iter()
            .any(|c| c.get(1).is_some_and(|v| v == "volume"))
    );
}
#[tokio::test]
async fn failed_replacement_start_restores_old_running_container_without_removing_volumes() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(MockRunner {
        calls: Mutex::new(vec![]),
        fail_create: false,
        fail_start_once: Mutex::new(true),
    });
    let docker = docker(dir.path(), runner.clone());
    assert!(docker.reset(1, &docker.config.gpus[&1]).await.is_err());
    let calls = runner.calls.lock().unwrap();
    assert_eq!(
        calls.last().unwrap(),
        &vec![
            "docker".to_owned(),
            "start".to_owned(),
            "jupiterhub_gpu1".to_owned()
        ]
    );
    assert!(calls.iter().any(|c| c.get(1).is_some_and(|v| v == "rename")
        && c.get(2).is_some_and(|v| v.contains("_previous_"))));
    assert!(!calls.iter().any(|c| c.contains(&"volume".into())
        || c.contains(&"-v".into()) && c.get(1).is_some_and(|v| v == "rm")));
}
#[tokio::test]
async fn failed_create_never_stops_renames_or_removes_existing_gpu() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(MockRunner {
        calls: Mutex::new(vec![]),
        fail_create: true,
        fail_start_once: Mutex::new(false),
    });
    let docker = docker(dir.path(), runner.clone());
    assert!(docker.reset(1, &docker.config.gpus[&1]).await.is_err());
    assert!(!runner.calls.lock().unwrap().iter().any(|c| {
        c.get(1)
            .is_some_and(|v| ["stop", "rename", "rm"].contains(&v.as_str()))
    }));
}

#[tokio::test]
async fn failed_initial_inspection_preserves_notebook_and_remembers_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    let at = Utc::now();
    let mut state = State::default();
    let driver = MockDriver {
        inspect_fail: true,
        ..Default::default()
    };
    let snapshot = Snapshot {
        passwords: BTreeMap::from([(1, "synthetic-password".into())]),
        reservations: vec![booking(
            "active",
            at - Duration::minutes(1),
            at + Duration::minutes(1),
        )],
        keys: Some(BTreeMap::new()),
    };
    let report = state.reconcile(&cfg, Some(snapshot), at, &driver).await;
    assert!(!report.errors.is_empty());
    assert!(driver.calls.lock().unwrap().is_empty());
    let report = state
        .reconcile(&cfg, None, at + Duration::minutes(2), &driver)
        .await;
    assert!(report.actions.iter().any(|action| action == "gpu1:reset"));
}

#[tokio::test]
async fn ssh_key_copy_repairs_root_ownership_after_copy() {
    let dir = tempfile::tempdir().unwrap();
    let runner = Arc::new(MockRunner {
        calls: Mutex::new(vec![]),
        fail_create: false,
        fail_start_once: Mutex::new(false),
    });
    let docker = docker(dir.path(), runner.clone());
    docker
        .write_keys(&docker.config.gpus[&1], &[])
        .await
        .unwrap();
    let calls = runner.calls.lock().unwrap();
    let copy = calls
        .iter()
        .position(|c| c.get(1).is_some_and(|v| v == "cp"))
        .unwrap();
    let repair = calls
        .iter()
        .position(|c| c.iter().any(|v| v == "chown"))
        .unwrap();
    assert!(repair > copy);
    assert_eq!(
        calls[repair],
        [
            "docker",
            "exec",
            "--user",
            "0:0",
            "jupiterhub_gpu1",
            "chown",
            "0:0",
            "/root/.ssh",
            "/root/.ssh/authorized_keys"
        ]
    );
    assert!(!calls.iter().any(|c| {
        c.iter()
            .any(|v| ["restart", "stop", "start"].contains(&v.as_str()))
    }));
}

/// Opt-in CPU-only Docker fixture. Never targets a configured GPU container.
#[tokio::test]
async fn native_key_sync_root_strictmodes_fixture() {
    if std::env::var("CBGPU_TEST_SSH_OWNERSHIP_DOCKER").as_deref() != Ok("1") {
        return;
    }
    use cbgpu_gpu_daemon::docker::NativeRunner;
    use std::process::Command;
    let name = format!("cbgpu-ssh-ownership-test-{}", std::process::id());
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
        }
    }
    let started = Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            &name,
            "--network",
            "none",
            "--memory",
            "256m",
            "--cpus",
            "1",
            "--entrypoint",
            "/bin/sh",
            "cbgpu/gpu-ssh:latest",
            "-c",
            "sleep 120",
        ])
        .output()
        .unwrap();
    assert!(started.status.success(), "CPU-only fixture failed to start");
    let _cleanup = Cleanup(name.clone());
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("fixture_ed25519");
    assert!(
        Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key_path)
            .status()
            .unwrap()
            .success()
    );
    let public = std::fs::read_to_string(key_path.with_extension("pub")).unwrap();
    let mut cfg = config(dir.path());
    cfg.gpus.get_mut(&1).unwrap().container = name.clone();
    let docker = Docker::new(cfg, "127.0.0.1".parse().unwrap(), Arc::new(NativeRunner)).unwrap();
    // Reproduce docker-cp's previous unprivileged owner, then repair via real adapter.
    assert!(Command::new("docker").args(["exec", "--user", "0:0", &name, "sh", "-c", "mkdir -p /root/.ssh; touch /root/.ssh/authorized_keys; chown 1000:1000 /root/.ssh/authorized_keys"]).status().unwrap().success());
    docker
        .write_keys(&docker.config.gpus[&1], std::slice::from_ref(&public))
        .await
        .unwrap();
    let result = Command::new("docker")
        .args([
            "exec",
            "--user",
            "0:0",
            &name,
            "stat",
            "-c",
            "%u:%g:%a",
            "/root/.ssh",
            "/root/.ssh/authorized_keys",
        ])
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        "0:0:700\n0:0:600\n"
    );
    let strict = Command::new("docker").args(["exec", "--user", "0:0", &name, "sh", "-c", "mkdir -p /run/sshd; ssh-keygen -A >/dev/null 2>&1; /usr/sbin/sshd -T -h /etc/ssh/ssh_host_ed25519_key"]).output().unwrap();
    assert!(
        strict.status.success(),
        "{}",
        String::from_utf8_lossy(&strict.stderr)
    );
    assert!(
        String::from_utf8(strict.stdout)
            .unwrap()
            .lines()
            .any(|l| l == "strictmodes yes")
    );
    assert!(
        Command::new("docker")
            .arg("cp")
            .arg(&key_path)
            .arg(format!("{name}:/tmp/fixture_ed25519"))
            .status()
            .unwrap()
            .success()
    );
    assert!(Command::new("docker").args(["exec", "--user", "0:0", &name, "sh", "-c", "chown 0:0 /tmp/fixture_ed25519; chmod 600 /tmp/fixture_ed25519; passwd -d root >/dev/null; /usr/sbin/sshd -f /dev/null -h /etc/ssh/ssh_host_ed25519_key -o StrictModes=yes -o PermitRootLogin=prohibit-password -o AuthorizedKeysFile=/root/.ssh/authorized_keys -o PidFile=/tmp/fixture_sshd.pid -p 2222"]).status().unwrap().success());
    let login = || {
        Command::new("docker")
            .args([
                "exec",
                "--user",
                "0:0",
                &name,
                "ssh",
                "-i",
                "/tmp/fixture_ed25519",
                "-p",
                "2222",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "ConnectTimeout=3",
                "root@127.0.0.1",
                "true",
            ])
            .output()
            .unwrap()
    };
    assert!(
        Command::new("docker")
            .args([
                "exec",
                "--user",
                "0:0",
                &name,
                "chown",
                "1000:1000",
                "/root/.ssh/authorized_keys"
            ])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        !login().status.success(),
        "StrictModes must reject the old unprivileged owner"
    );
    docker
        .write_keys(&docker.config.gpus[&1], &[public])
        .await
        .unwrap();
    let accepted = login();
    assert!(
        accepted.status.success(),
        "StrictModes login after repair: {}",
        String::from_utf8_lossy(&accepted.stderr)
    );
}

#[tokio::test]
async fn fresh_same_booking_extension_preserves_runtime_across_original_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    let at = Utc::now();
    let old_end = at + Duration::seconds(30);
    let new_end = old_end + Duration::minutes(30);
    let driver = MockDriver {
        configured: true,
        ..Default::default()
    };
    let mut state = State::default();
    let mut snapshot = Snapshot {
        passwords: BTreeMap::new(),
        keys: Some(BTreeMap::new()),
        reservations: vec![booking("same-booking", at - Duration::minutes(1), old_end)],
    };
    state
        .reconcile(&cfg, Some(snapshot.clone()), at, &driver)
        .await;
    snapshot.reservations[0].end_time = new_end;
    state
        .reconcile(
            &cfg,
            Some(snapshot.clone()),
            old_end - Duration::milliseconds(1),
            &driver,
        )
        .await;
    state
        .reconcile(&cfg, None, old_end + Duration::seconds(1), &driver)
        .await;
    assert_eq!(state.active[&1].deadline, new_end);
    assert!(
        !driver
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.starts_with("reset"))
    );
    // An unavailable authority still cannot move the last verified deadline.
    state.reconcile(&cfg, None, new_end, &driver).await;
    assert!(state.active.is_empty());
    assert!(
        driver
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "reset1")
    );
}
