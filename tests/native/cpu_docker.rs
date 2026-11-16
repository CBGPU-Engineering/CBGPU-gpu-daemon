//! Opt-in real Docker lifecycle fixture. Names, volumes and loopback port belong only to this test.
use cbgpu_gpu_daemon::{
    authorization::Result,
    cpu::{Engine, Project},
    docker::{NativeRunner, Output, Runner},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
struct Fixture {
    prefix: String,
    volumes: Arc<Mutex<Vec<String>>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let r = std::process::Command::new("docker")
            .args(["ps", "-aq", "--filter", &format!("name=^/{}", self.prefix)])
            .output()
            .unwrap();
        for id in String::from_utf8_lossy(&r.stdout).lines() {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", id])
                .output();
        }
        for volume in self.volumes.lock().unwrap().iter() {
            let _ = std::process::Command::new("docker")
                .args(["volume", "rm", volume])
                .output();
        }
    }
}
struct TrackingRunner {
    volumes: Arc<Mutex<Vec<String>>>,
}
#[async_trait::async_trait]
impl Runner for TrackingRunner {
    async fn run(&self, program: &str, args: &[String], input: Option<Vec<u8>>) -> Result<Output> {
        let result = NativeRunner.run(program, args, input).await?;
        if program == "docker" && args.first().is_some_and(|a| a == "create") && result.success {
            let id = result.stdout.trim();
            let mounts = NativeRunner
                .run(
                    "docker",
                    &[
                        "inspect".into(),
                        "--format".into(),
                        "{{range .Mounts}}{{if eq .Type \"volume\"}}{{.Name}}\n{{end}}{{end}}"
                            .into(),
                        id.into(),
                    ],
                    None,
                )
                .await?;
            self.volumes.lock().unwrap().extend(
                mounts
                    .stdout
                    .lines()
                    .filter(|s| !s.is_empty())
                    .map(String::from),
            );
        }
        Ok(result)
    }
}
#[tokio::test]
#[ignore = "Creates only its own disposable CPU containers/volumes and an ephemeral loopback port; no live services or DB"]
async fn real_cpu_deploy_health_failure_and_rollback_keep_original_service() {
    use rand::RngCore;
    let mut bytes = [0; 4];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let prefix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let id = format!("{prefix}-2222-4333-8444-555555555555");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let volumes = Arc::new(Mutex::new(Vec::new()));
    let fixture = Fixture {
        prefix: format!("cbcpu_{prefix}"),
        volumes: volumes.clone(),
    };
    let mut p:Project=serde_json::from_value(serde_json::json!({"id":id,"status":"approved","host_port":port,"container_port":5432,"cpu_limit":1.0,"memory_limit":"256m","access_scope":"private","deploy_type":"docker","docker_image":"postgres:17-alpine","env_vars":{"POSTGRES_PASSWORD":"fixture-private-password"}})).unwrap();
    let engine = Engine {
        runner: Arc::new(TrackingRunner { volumes }),
        school: "10.138.23.12".parse().unwrap(),
        private: "127.0.0.1".parse().unwrap(),
        tunnel_image: "cloudflare/cloudflared:latest".into(),
        poll: Duration::from_millis(100),
        startup_timeout: Duration::from_secs(30),
    };
    engine.deploy(&p).await.unwrap();
    assert!(engine.running(&p).await.unwrap());
    assert!(
        tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .is_ok()
    );
    let before = std::process::Command::new("docker")
        .args(["inspect", "--format", "{{.Id}}", &fixture.prefix])
        .output()
        .unwrap();
    assert!(before.status.success());
    p.env_vars.insert(
        "POSTGRES_INITDB_ARGS".into(),
        "--cbgpu-invalid-fixture".into(),
    );
    let failure = engine.deploy(&p).await.unwrap_err();
    assert!(failure.rollback_restored);
    assert!(!failure.logs.contains("fixture-private-password"));
    assert!(engine.running(&p).await.unwrap());
    let after = std::process::Command::new("docker")
        .args(["inspect", "--format", "{{.Id}}", &fixture.prefix])
        .output()
        .unwrap();
    assert_eq!(before.stdout, after.stdout);
    assert!(
        tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .is_ok()
    );
    drop(fixture);
}
