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
