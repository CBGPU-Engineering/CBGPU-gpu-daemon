use cbgpu_gpu_daemon::{
    authorization::Result,
    cpu::{Engine, Project, collision_prefixes, memory_bytes, tunnel_url},
    docker::{Output, Runner},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
fn project() -> Project {
    serde_json::from_value(serde_json::json!({"id":"11111111-2222-4333-8444-555555555555","user_id":"owner","name":"fixture","status":"approved","host_port":31555,"container_port":8080,"cpu_limit":3.0,"memory_limit":"16g","access_scope":"public","deploy_type":"git","git_url":"https://github.com/example/repository.git","git_branch":"main","env_vars":{"CBGPU_RUNTIME_PRESET":"node22","CBGPU_ROOT_DIRECTORY":"server","DATABASE_PASSWORD":"SYNTHETIC_PRIVATE_PASSWORD"}})).unwrap()
}
#[derive(Default)]
struct Mock {
    calls: Mutex<Vec<(String, Vec<String>)>>,
    old: bool,
    fail_create: bool,
    fail_start: bool,
    unhealthy: bool,
    secret_seen: Mutex<bool>,
}
#[async_trait::async_trait]
impl Runner for Mock {
    async fn run(&self, program: &str, args: &[String], _: Option<Vec<u8>>) -> Result<Output> {
        self.calls
            .lock()
            .unwrap()
            .push((program.into(), args.into()));
        let mut output = Output {
            success: true,
            stdout: String::new(),
            stderr: String::new(),
        };
        if args.first().is_some_and(|a| a == "create") {
            if let Some(i) = args.iter().position(|a| a == "--env-file") {
                *self.secret_seen.lock().unwrap() = std::fs::read_to_string(&args[i + 1])
                    .unwrap()
                    .contains("SYNTHETIC_PRIVATE_PASSWORD");
            }
            if self.fail_create {
                output.success = false;
            }
        }
        if args.first().is_some_and(|a| a == "inspect") {
            let template = &args[2];
            if template == "{{.State.Running}}" {
                output.success = self.old;
                output.stdout = "true".into();
                if !self.old {
                    output.stderr = "No such object".into();
                }
            } else if template.contains(".Mounts") {
                output.stdout = "existing_persistent_volume".into();
            } else if template.contains("RestartCount") {
                output.stdout = if self.unhealthy {
                    "true|0|unhealthy"
                } else {
                    "true|0|healthy"
                }
                .into();
            } else if template.contains("cbgpu.project") {
                output.stdout = "11111111-2222-4333-8444-555555555555".into();
            } else if template.contains(".State.Health") {
                output.stdout = "health: SYNTHETIC_PRIVATE_PASSWORD".into();
            } else {
                output.stdout = "true|31555".into();
            }
        }
        if args.first().is_some_and(|a| a == "start") && self.fail_start {
            output.success = false;
        }
        if args.first().is_some_and(|a| a == "logs") {
            output.stdout =
                "app log SYNTHETIC_PRIVATE_PASSWORD https://sample-robot.trycloudflare.com".into();
        }
        if program == "ufw" {
            output.stdout = "Status: active".into();
        }
        Ok(output)
    }
}
fn engine(mock: Arc<Mock>) -> Engine {
    Engine {
        runner: mock,
        school: "10.138.23.12".parse().unwrap(),
        private: "100.100.136.37".parse().unwrap(),
        tunnel_image: "cloudflare/cloudflared:latest".into(),
        poll: Duration::ZERO,
        startup_timeout: Duration::from_millis(50),
    }
}
#[test]
fn runtime_and_root_directory_match_supported_legacy_presets() {
    let mut p = project();
    for runtime in [
        "node22",
        "node20",
        "node18",
        "python312",
        "python311",
        "python39",
        "java21",
        "java17",
        "go1",
        "dotnet10",
    ] {
        p.env_vars
            .insert("CBGPU_RUNTIME_PRESET".into(), runtime.into());
        let git = p.git_configuration().unwrap();
        assert!(git.script.contains("git -C /app fetch origin 'main'"));
        assert!(git.script.contains("cd '/app/server'"));
        assert!(git.script.contains("pwd -P"));
    }
    for root in [
        "../secret",
        "/root",
        "server/../../",
        "C:\\escape",
        "bad\nroot",
    ] {
        p.env_vars
            .insert("CBGPU_ROOT_DIRECTORY".into(), root.into());
        assert!(p.git_configuration().is_err());
    }
    p.env_vars
        .insert("CBGPU_ROOT_DIRECTORY".into(), "server's code".into());
    assert!(
        p.git_configuration()
            .unwrap()
            .script
            .contains("server'\\''s code")
    );
    p.git_url = Some("https://token@github.com/example/private".into());
    assert!(p.git_configuration().is_err());
}
#[test]
fn memory_ceiling_and_scopes_are_enforced() {
    assert_eq!(memory_bytes("16g").unwrap(), 16 << 30);
    for invalid in ["17g", "0", "-1g", "1;rm -rf /", "999999999999999999999999g"] {
        assert!(memory_bytes(invalid).is_err());
    }
    let mut p = project();
    p.env_vars
        .insert("CBGPU_DEPLOY_ENGINE".into(), "compose".into());
    assert!(p.validate().is_err());
}
#[test]
fn prefix_collisions_are_denied_and_last_actual_tunnel_url_wins() {
    let a = project();
    let mut b = a.clone();
    b.id = "11111111-9999-4333-8444-555555555555".into();
    assert!(collision_prefixes(&[a, b]).contains("11111111"));
    assert_eq!(
        tunnel_url(
            "https://old-url.trycloudflare.com https://api.trycloudflare.com https://new-url.trycloudflare.com"
        ),
        Some("https://new-url.trycloudflare.com".into())
    );
    assert_eq!(tunnel_url("https://api.trycloudflare.com"), None);
}
#[tokio::test]
async fn stages_before_stop_clones_storage_redacts_env_and_keeps_rollback() {
    let mock = Arc::new(Mock {
        old: true,
        ..Default::default()
    });
    engine(mock.clone()).deploy(&project()).await.unwrap();
    let calls = mock.calls.lock().unwrap();
    let args: Vec<_> = calls.iter().map(|(_, a)| a).collect();
    let create = args.iter().position(|a| a[0] == "create").unwrap();
    let stop = args.iter().position(|a| a[0] == "stop").unwrap();
    assert!(create < stop);
    assert!(*mock.secret_seen.lock().unwrap());
    let encoded = serde_json::to_string(&*calls).unwrap();
    assert!(!encoded.contains("SYNTHETIC_PRIVATE_PASSWORD"));
    assert!(!encoded.contains("0.0.0.0:"));
    assert!(!encoded.contains("docker.sock"));
    assert!(!encoded.contains("--privileged"));
    assert!(args[create].windows(2).any(|v| v == ["--cap-drop", "ALL"]));
    assert!(args.iter().any(|a| {
        a.windows(2).any(|v| v == ["--network", "none"])
            && a.iter()
                .any(|v| v == "existing_persistent_volume:/source:ro")
    }));
    assert!(
        !args
            .iter()
            .any(|a| a[0] == "rm" || a.windows(2).any(|v| v == ["volume", "rm"]))
    );
}
#[tokio::test]
async fn create_failure_leaves_live_service_and_storage_untouched() {
    let mock = Arc::new(Mock {
        old: true,
        fail_create: true,
        ..Default::default()
    });
    assert!(engine(mock.clone()).deploy(&project()).await.is_err());
    assert!(
        !mock
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(_, a)| ["stop", "rename", "rm"].contains(&a[0].as_str()))
    );
}
#[tokio::test]
async fn unhealthy_service_preserves_diagnostics_restores_old_and_keeps_all_volumes() {
    let mock = Arc::new(Mock {
        old: true,
        unhealthy: true,
        ..Default::default()
    });
    let result = engine(mock.clone()).deploy(&project()).await.unwrap_err();
    assert!(result.rollback_restored);
    assert!(result.logs.contains("[REDACTED]"));
    assert!(!result.health.contains("SYNTHETIC_PRIVATE_PASSWORD"));
    let calls = mock.calls.lock().unwrap();
    assert!(
        calls.iter().any(|(_, a)| a[0] == "rename"
            && a[1].contains("previous")
            && a[2] == "cbcpu_11111111")
    );
    assert!(
        !calls
            .iter()
            .any(|(_, a)| a.windows(2).any(|v| v == ["volume", "rm"]))
    );
}
#[tokio::test]
async fn private_access_never_uses_public_tunnel_or_public_interface() {
    let mock = Arc::new(Mock {
        old: true,
        ..Default::default()
    });
    let mut p = project();
    p.access_scope = "private".into();
    engine(mock.clone()).deploy(&p).await.unwrap();
    assert_eq!(engine(mock.clone()).sync_tunnel(&p).await.unwrap(), None);
    let calls = mock.calls.lock().unwrap();
    let create = &calls.iter().find(|(_, a)| a[0] == "create").unwrap().1;
    let bindings: Vec<_> = create
        .windows(2)
        .filter(|v| v[0] == "-p")
        .map(|v| &v[1])
        .collect();
    assert_eq!(bindings, vec!["100.100.136.37:31555:8080"]);
    assert!(!calls.iter().any(|(program, _)| program == "ufw"));
}
#[tokio::test]
async fn deleted_project_removes_only_owned_containers_and_keeps_volumes() {
    let mock = Arc::new(Mock::default());
    let mut p = project();
    p.status = "deleted".into();
    p.access_scope = "private".into();
    engine(mock.clone()).remove(&p).await.unwrap();
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].1, vec!["rm", "-f", "cbcpu_11111111"]);
    assert_eq!(calls[1].1, vec!["rm", "-f", "cbtun_11111111"]);
}

#[test]
fn legacy_postgrest_text_cpu_limit_is_supported_without_changing_project_identity() {
    let original = project();
    let mut row = serde_json::to_value(&original).unwrap();
    row["cpu_limit"] = serde_json::json!("3.0");
    let restored: Project = serde_json::from_value(row.clone()).unwrap();
    restored.validate().unwrap();
    assert_eq!(restored.id, original.id);
    assert_eq!(restored.user_id, original.user_id);
    assert_eq!(restored.cpu_limit, 3.0);
    row["cpu_limit"] = serde_json::json!("--privileged");
    assert!(serde_json::from_value::<Project>(row).is_err());
}
#[test]
fn healthy_public_tunnel_maintenance_cannot_starve_new_deployments_or_other_projects() {
    let mut first = project();
    first.deployed_at = Some(chrono::Utc::now());
    first.deploy_requested_at = None;
    let mut second = first.clone();
    second.id = "22222222-2222-4333-8444-555555555555".into();
    let mut third = first.clone();
    third.id = "33333333-2222-4333-8444-555555555555".into();
    let mut pending = first.clone();
    pending.id = "44444444-2222-4333-8444-555555555555".into();
    pending.deployed_at = None;
    let mut rows = vec![
        first.clone(),
        second.clone(),
        third.clone(),
        pending.clone(),
    ];
    cbgpu_gpu_daemon::cpu::scheduling_order(&mut rows, Some(&second.id));
    assert_eq!(
        rows.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        vec![
            pending.id.as_str(),
            third.id.as_str(),
            first.id.as_str(),
            second.id.as_str()
        ]
    );
}
