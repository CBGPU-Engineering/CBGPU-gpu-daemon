//! Non-Compose CPU deployment engine. Persistent volumes and previous containers are never deleted.
//! All process invocations use argument vectors; user commands run only inside restricted containers.
use crate::{
    authorization::{Failure, Result},
    docker::{Output, Runner},
};
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::Ipv4Addr,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, Deserialize, Serialize)]
pub struct Project {
    pub id: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub name: String,
    pub status: String,
    pub host_port: u16,
    pub container_port: u16,
    #[serde(deserialize_with = "deserialize_cpu_limit")]
    pub cpu_limit: f64,
    pub memory_limit: String,
    #[serde(default = "private")]
    pub access_scope: String,
    #[serde(default = "docker")]
    pub deploy_type: String,
    #[serde(default)]
    pub docker_image: Option<String>,
    #[serde(default)]
    pub git_url: Option<String>,
    #[serde(default)]
    pub git_branch: Option<String>,
    #[serde(default)]
    pub run_command: Option<String>,
    #[serde(default)]
    pub runtime: Option<String>,
    #[serde(default)]
    pub env_vars: BTreeMap<String, String>,
    #[serde(default)]
    pub deployed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub deploy_requested_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub public_url: Option<String>,
}
fn deserialize_cpu_limit<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<f64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Limit {
        Text(String),
        Number(f64),
    }
    match Limit::deserialize(deserializer)? {
        Limit::Text(value) => value.parse().map_err(serde::de::Error::custom),
        Limit::Number(value) => Ok(value),
    }
}
fn private() -> String {
    "private".into()
}
fn docker() -> String {
    "docker".into()
}
fn identifier(s: &str) -> bool {
    s.len() == 36
        && s.chars().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
pub fn container_name(id: &str) -> Result<String> {
    if !identifier(id) {
        return Err(Failure("Invalid CPU project identity"));
    }
    Ok(format!("cbcpu_{}", &id[..8]))
}
pub fn collision_prefixes(projects: &[Project]) -> BTreeSet<String> {
    let mut owners: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for p in projects {
        if identifier(&p.id) {
            owners
                .entry(p.id[..8].into())
                .or_default()
                .insert(p.id.clone());
        }
    }
    owners
        .into_iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(prefix, _)| prefix)
        .collect()
}
/// Prioritize requested changes and rotate healthy tunnel maintenance so it cannot starve other projects.
pub fn scheduling_order(projects: &mut [Project], last_scheduled: Option<&str>) {
    projects.sort_by_key(|p| {
        let urgent = p.status == "deleted"
            || p.deployed_at.is_none()
            || p.deploy_requested_at
                .is_some_and(|request| p.deployed_at.is_none_or(|deployed| request > deployed));
        (
            !urgent,
            last_scheduled.is_some_and(|last| p.id.as_str() <= last),
            p.id.clone(),
        )
    });
}
fn image_valid(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._:@-".contains(c))
}
pub fn memory_bytes(s: &str) -> Result<u64> {
    let lower = s.trim().to_ascii_lowercase();
    let (number, multiplier) = if let Some(n) = lower.strip_suffix("gb") {
        (n, 1u64 << 30)
    } else if let Some(n) = lower.strip_suffix('g') {
        (n, 1 << 30)
    } else if let Some(n) = lower.strip_suffix("mb") {
        (n, 1 << 20)
    } else if let Some(n) = lower.strip_suffix('m') {
        (n, 1 << 20)
    } else {
        (lower.as_str(), 1)
    };
    let value = number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .ok_or(Failure("Invalid CPU memory limit"))?;
    if !(64 * 1024 * 1024..=16u64 * 1024 * 1024 * 1024).contains(&value) {
        return Err(Failure("CPU memory must be between 64 MB and 16 GB"));
    }
    Ok(value)
}
/// Build pyproject packages from a disposable tracked Git snapshot. The persistent
/// application checkout (including untracked data and prior build outputs) is untouched.
/// Requirements installs retain their existing editable/local-path behavior.
pub fn python_dependency_setup() -> &'static str {
    r#"if [ -f requirements.txt ]; then
    pip install -r requirements.txt -q;
elif [ -f pyproject.toml ]; then
    repository_root=$(git rev-parse --show-toplevel);
    python_build=$(mktemp -d /tmp/cbgpu-python-build.XXXXXX);
    trap 'rm -rf -- "$python_build"' EXIT;
    git clone --quiet --no-hardlinks --no-checkout "$repository_root" "$python_build";
    git -C "$python_build" checkout --quiet --detach "$(git rev-parse HEAD)";
    (cd "$python_build${project_root#"$repository_root"}" && pip install . -q);
    rm -rf -- "$python_build";
    trap - EXIT;
fi; "#
}
impl Project {
    pub fn validate(&self) -> Result<()> {
        container_name(&self.id)?;
        if self.host_port < 1024
            || self.container_port == 0
            || !self.cpu_limit.is_finite()
            || !(0.1..=64.0).contains(&self.cpu_limit)
        {
            return Err(Failure("Invalid CPU resource or port allocation"));
        }
        memory_bytes(&self.memory_limit)?;
        if !["private", "school", "public"].contains(&self.access_scope.as_str())
            || !["approved", "deleted"].contains(&self.status.as_str())
        {
            return Err(Failure("CPU project is not eligible for deployment"));
        }
        if self.env_vars.len() > 200
            || self.env_vars.iter().any(|(k, v)| {
                k.is_empty()
                    || k.len() > 200
                    || k.starts_with(|c: char| !c.is_ascii_alphabetic() && c != '_')
                    || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    || v.contains('\0')
                    || v.len() > 65536
            })
        {
            return Err(Failure("Invalid deployment environment"));
        }
        if self
            .env_vars
            .get("CBGPU_DEPLOY_ENGINE")
            .is_some_and(|v| v == "compose")
        {
            return Err(Failure("Compose deployments belong to the Compose worker"));
        }
        if self.deploy_type == "docker" {
            if !self.docker_image.as_deref().is_some_and(image_valid) {
                return Err(Failure("Invalid CPU Docker image"));
            }
        } else if self.deploy_type == "git" {
            self.git_configuration()?;
        } else {
            return Err(Failure("Unsupported CPU deployment type"));
        }
        Ok(())
    }
    pub fn git_configuration(&self) -> Result<GitConfiguration> {
        let runtime = self
            .env_vars
            .get("CBGPU_RUNTIME_PRESET")
            .map(String::as_str)
            .or(self.runtime.as_deref())
            .unwrap_or("node22");
        let image = match runtime {
            "node22" => "node:22-alpine",
            "node20" => "node:20-alpine",
            "node18" => "node:18-alpine",
            "python312" => "python:3.12-slim",
            "python311" => "python:3.11-slim",
            "python39" => "python:3.9-slim",
            "java21" => "maven:3.9-eclipse-temurin-21",
            "java17" => "maven:3.9-eclipse-temurin-17",
            "go1" => "golang:1-alpine",
            "dotnet10" => "mcr.microsoft.com/dotnet/sdk:10.0",
            _ => return Err(Failure("Unsupported server runtime")),
        };
        let root = self
            .env_vars
            .get("CBGPU_ROOT_DIRECTORY")
            .map(|s| s.trim())
            .unwrap_or(".");
        if root.len() > 200
            || root.starts_with('/')
            || root.contains(['\\', '\0', '\r', '\n'])
            || root.split('/').any(|part| part == "..")
        {
            return Err(Failure("Root Directory must remain inside repository"));
        }
        let url = self
            .git_url
            .as_deref()
            .ok_or(Failure("Git repository URL required"))?;
        let parsed = reqwest::Url::parse(url).map_err(|_| Failure("Invalid Git repository URL"))?;
        if parsed.scheme() != "https"
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || url.len() > 2000
        {
            return Err(Failure(
                "Git repository requires HTTPS without embedded credentials",
            ));
        }
        let branch = self
            .git_branch
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("main");
        if branch.starts_with('-') || branch.len() > 200 || branch.contains(['\0', '\r', '\n']) {
            return Err(Failure("Invalid Git branch"));
        }
        let default = match runtime {
            "java21" | "java17" => "mvn -B -DskipTests package && java -jar target/*.jar",
            "go1" => "go build -o /tmp/cbgpu-server . && exec /tmp/cbgpu-server",
            "dotnet10" => {
                "dotnet run --configuration Release --no-launch-profile --urls http://0.0.0.0:8080"
            }
            _ if runtime.starts_with("python") => "python app.py",
            _ => "npm start",
        };
        let run = self
            .run_command
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(default);
        if run.len() > 16000 || run.contains('\0') {
            return Err(Failure("Invalid container start command"));
        }
        let install = if runtime.starts_with("node") || runtime == "go1" {
            "apk add --no-cache git; "
        } else {
            "apt-get update -qq; apt-get install -y -qq git; "
        };
        let deps = if runtime.starts_with("python") {
            python_dependency_setup()
        } else if runtime.starts_with("node") {
            "if [ -f package-lock.json ]; then npm ci; elif [ -f package.json ]; then npm install; fi; "
        } else {
            ""
        };
        let script = format!(
            "set -e; rm -f /tmp/cbgpu-ready; {install}if [ ! -d /app/.git ]; then git clone -- {} /app; fi; git -C /app remote set-url origin {}; git -C /app fetch origin {}; git -C /app checkout -B {} FETCH_HEAD; cd {}; project_root=$(pwd -P); case \"$project_root\" in /app|/app/*) ;; *) echo 'Root Directory escapes repository'; exit 1 ;; esac; {deps}touch /tmp/cbgpu-ready; exec sh -c {}",
            quote(url),
            quote(url),
            quote(branch),
            quote(branch),
            quote(&format!("/app/{root}")),
            quote(run)
        );
        Ok(GitConfiguration {
            image: image.into(),
            script,
        })
    }
}
pub struct GitConfiguration {
    pub image: String,
    pub script: String,
}
#[derive(Clone)]
pub struct Engine {
    pub runner: Arc<dyn Runner>,
    pub school: Ipv4Addr,
    pub private: Ipv4Addr,
    pub tunnel_image: String,
    pub poll: Duration,
    pub startup_timeout: Duration,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentFailure {
    pub message: String,
    pub logs: String,
    pub health: String,
    pub rollback_restored: bool,
}
impl Engine {
    pub fn validate(&self) -> Result<()> {
        if !self.school.is_private()
            || self.school.is_loopback()
            || self.school.is_unspecified()
            || self.private.is_unspecified()
            || (!self.private.is_private()
                && !self.private.is_loopback()
                && !(self.private.octets()[0] == 100
                    && (64..=127).contains(&self.private.octets()[1])))
            || !image_valid(&self.tunnel_image)
        {
            return Err(Failure("Invalid CPU deployment networking"));
        }
        Ok(())
    }
    async fn docker(&self, args: Vec<String>) -> Result<Output> {
        self.runner.run("docker", &args, None).await
    }
    async fn require(&self, args: Vec<String>, message: &'static str) -> Result<Output> {
        let r = self.docker(args).await?;
        if !r.success {
            return Err(Failure(message));
        }
        Ok(r)
    }
    pub async fn running(&self, p: &Project) -> Result<bool> {
        let r = self
            .docker(vec![
                "inspect".into(),
                "--format".into(),
                "{{.State.Running}}".into(),
                container_name(&p.id)?,
            ])
            .await?;
        if r.success {
            return Ok(r.stdout.trim() == "true");
        }
        if r.stderr.to_lowercase().contains("no such object")
            || r.stderr.to_lowercase().contains("no such container")
        {
            return Ok(false);
        }
        Err(Failure("Cannot inspect existing CPU container"))
    }
    async fn firewall(&self, p: &Project, delete: bool) -> Result<()> {
        if p.access_scope == "private" || (p.access_scope == "public" && !delete) {
            return Ok(());
        }
        let status = self.runner.run("ufw", &["status".into()], None).await?;
        if !status.success || !status.stdout.starts_with("Status: active") {
            return Err(Failure("UFW must be active for school access"));
        }
        if delete
            && p.access_scope == "public"
            && !status.stdout.lines().any(|l| {
                l.split_whitespace().next().is_some_and(|v| {
                    v == p.host_port.to_string() || v == format!("{}/tcp", p.host_port)
                })
            })
        {
            return Ok(());
        }
        let mut args = Vec::new();
        if delete {
            args.push("delete".into());
        }
        args.push("allow".into());
        if p.access_scope == "school" {
            args.extend([
                "from".into(),
                "10.138.26.0/24".into(),
                "to".into(),
                "any".into(),
                "port".into(),
                p.host_port.to_string(),
            ]);
        } else {
            args.push(p.host_port.to_string());
        }
        if !self.runner.run("ufw", &args, None).await?.success {
            return Err(Failure("Cannot update CPU firewall rule"));
        }
        Ok(())
    }
    pub async fn remove(&self, p: &Project) -> Result<()> {
        p.validate()?;
        self.validate()?;
        self.remove_container(&container_name(&p.id)?).await?;
        self.remove_container(&format!("cbtun_{}", &p.id[..8]))
            .await?;
        self.firewall(p, true).await
    }
    async fn remove_container(&self, name: &str) -> Result<()> {
        let r = self
            .docker(vec!["rm".into(), "-f".into(), name.into()])
            .await?;
        if r.success || r.stderr.to_lowercase().contains("no such container") {
            Ok(())
        } else {
            Err(Failure("Cannot remove scoped CPU container"))
        }
    }
    pub fn redact(&self, p: &Project, text: &str) -> String {
        let mut output = text.to_owned();
        let mut values: Vec<&str> = p
            .env_vars
            .values()
            .filter(|v| !v.is_empty())
            .map(String::as_str)
            .collect();
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));
        for value in values {
            output = output.replace(value, "[REDACTED]");
        }
        output
            .chars()
            .rev()
            .take(24000)
            .collect::<String>()
            .chars()
            .rev()
            .collect()
    }
    async fn diagnostics(
        &self,
        p: &Project,
        name: &str,
        message: &str,
        restored: bool,
    ) -> DeploymentFailure {
        let logs = self
            .docker(vec![
                "logs".into(),
                "--timestamps".into(),
                "--tail".into(),
                "200".into(),
                name.into(),
            ])
            .await
            .ok()
            .map(|r| format!("{}{}", r.stdout, r.stderr))
            .unwrap_or_default();
        let health = self
            .docker(vec![
                "inspect".into(),
                "--format".into(),
                "{{json .State.Health}}".into(),
                name.into(),
            ])
            .await
            .ok()
            .map(|r| r.stdout)
            .unwrap_or_default();
        DeploymentFailure {
            message: message.into(),
            logs: self.redact(p, &logs),
            health: self.redact(p, &health),
            rollback_restored: restored,
        }
    }
    async fn healthy(&self, name: &str, git: bool) -> Result<()> {
        let started = Instant::now();
        let mut ready = None;
        loop {
            let state=self.require(vec!["inspect".into(),"--format".into(),"{{.State.Running}}|{{.RestartCount}}|{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}".into(),name.into()],"Cannot inspect CPU startup").await?;
            let parts: Vec<_> = state.stdout.trim().split('|').collect();
            if parts.len() != 3 || parts[0] != "true" || parts[1] != "0" || parts[2] == "unhealthy"
            {
                return Err(Failure(
                    "CPU container exited, restarted or became unhealthy",
                ));
            }
            let mark = if git {
                self.docker(vec![
                    "exec".into(),
                    name.into(),
                    "test".into(),
                    "-f".into(),
                    "/tmp/cbgpu-ready".into(),
                ])
                .await?
                .success
            } else {
                true
            };
            if mark && (parts[2] == "healthy" || parts[2] == "none") {
                let since = ready.get_or_insert_with(Instant::now);
                if since.elapsed()
                    >= if self.poll.is_zero() {
                        Duration::ZERO
                    } else {
                        Duration::from_secs(if git { 3 } else { 10 })
                    }
                {
                    return Ok(());
                }
            } else {
                ready = None;
            }
            if started.elapsed() >= self.startup_timeout {
                return Err(Failure("CPU startup timed out"));
            }
            tokio::time::sleep(self.poll).await;
        }
    }
    pub async fn deploy(&self, p: &Project) -> std::result::Result<(), DeploymentFailure> {
        if let Err(error) = p.validate().and_then(|_| self.validate()) {
            return Err(DeploymentFailure {
                message: error.0.into(),
                logs: String::new(),
                health: String::new(),
                rollback_restored: true,
            });
        }
        if let Err(error) = self.firewall(p, false).await {
            return Err(DeploymentFailure {
                message: error.0.into(),
                logs: String::new(),
                health: String::new(),
                rollback_restored: true,
            });
        }
        let name = container_name(&p.id).expect("validated identity");
        let mut bytes = [0u8; 8];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let candidate = format!("{name}_next_{suffix}");
        let backup = format!("{name}_previous_{suffix}");
        let result = self.stage(p, &name, &candidate, &backup).await;
        match result {
            Ok(()) => Ok(()),
            Err((error, diagnostics)) => {
                let mut failure = diagnostics.unwrap_or(DeploymentFailure {
                    message: String::new(),
                    logs: String::new(),
                    health: String::new(),
                    rollback_restored: true,
                });
                failure.message = error.0.into();
                Err(failure)
            }
        }
    }
    async fn stage(
        &self,
        p: &Project,
        name: &str,
        candidate: &str,
        backup: &str,
    ) -> std::result::Result<(), (Failure, Option<DeploymentFailure>)> {
        macro_rules! required {
            ($future:expr) => {
                $future.await.map_err(|e| (e, None))?
            };
        }
        let inspection = required!(self.docker(vec![
            "inspect".into(),
            "--format".into(),
            "{{.State.Running}}".into(),
            name.into()
        ]));
        if !inspection.success
            && !inspection.stderr.to_lowercase().contains("no such object")
            && !inspection
                .stderr
                .to_lowercase()
                .contains("no such container")
        {
            return Err((Failure("Cannot inspect existing CPU deployment"), None));
        }
        let old = inspection.success;
        if old {
            let ownership = required!(self.require(
                vec![
                    "inspect".into(),
                    "--format".into(),
                    "{{index .Config.Labels \"cbgpu.project\"}}".into(),
                    name.into()
                ],
                "Cannot inspect existing CPU project ownership"
            ));
            let label = ownership.stdout.trim();
            if !label.is_empty() && label != "<no value>" && label != p.id {
                return Err((
                    Failure("Existing CPU container belongs to a different project"),
                    None,
                ));
            }
        }
        let running = old && inspection.stdout.trim() == "true";
        let mut args = vec![
            "create".into(),
            "--name".into(),
            candidate.into(),
            "--cpus".into(),
            p.cpu_limit.to_string(),
            "-m".into(),
            p.memory_limit.clone(),
            "--restart".into(),
            "unless-stopped".into(),
            "--security-opt".into(),
            "no-new-privileges:true".into(),
            "--cap-drop".into(),
            "ALL".into(),
            "--pids-limit".into(),
            "512".into(),
            "--label".into(),
            format!("cbgpu.project={}", p.id),
            "--label".into(),
            "cbgpu.engine=native-cpu".into(),
        ];
        // Runtime package managers and standard image entrypoints need these file/identity capabilities.
        // None grants host administration, device access or Docker socket access.
        for capability in ["CHOWN", "DAC_OVERRIDE", "FOWNER", "SETGID", "SETUID"] {
            args.extend(["--cap-add".into(), capability.into()]);
        }
        if p.container_port < 1024 {
            args.extend(["--cap-add".into(), "NET_BIND_SERVICE".into()]);
        }
        let addresses: BTreeSet<Ipv4Addr> = match p.access_scope.as_str() {
            "private" => [self.private].into(),
            "school" => [self.school].into(),
            _ => [Ipv4Addr::LOCALHOST, self.school, self.private].into(),
        };
        for address in addresses {
            args.extend([
                "-p".into(),
                format!("{address}:{}:{}", p.host_port, p.container_port),
            ]);
        }
        // Values stay off process arguments and diagnostic command output.
        let staging = required!(async {
            tempfile::tempdir().map_err(|_| Failure("Cannot stage deployment environment"))
        });
        let env_path = staging.path().join("environment");
        let mut environment = String::new();
        for (k, v) in &p.env_vars {
            if v.contains(['\n', '\r']) {
                return Err((
                    Failure("Environment values cannot contain line breaks"),
                    None,
                ));
            }
            environment.push_str(&format!("{k}={v}\n"));
        }
        required!(async {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&env_path)
                .map_err(|_| Failure("Cannot stage private environment"))?;
            f.write_all(environment.as_bytes())
                .map_err(|_| Failure("Cannot stage private environment"))
        });
        args.extend(["--env-file".into(), env_path.to_string_lossy().into_owned()]);
        let mut source = None;
        let mut copy_image = None;
        let mut staged = None;
        if p.deploy_type == "git" {
            let git = p.git_configuration().map_err(|e| (e, None))?;
            let volume = if old {
                let mount=required!(self.require(vec!["inspect".into(),"--format".into(),"{{range .Mounts}}{{if eq .Destination \"/app\"}}{{.Name}}{{end}}{{end}}".into(),name.into()],"Cannot inspect existing project storage"));
                let mount = mount.stdout.trim();
                if mount.is_empty()
                    || !mount
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
                {
                    return Err((
                        Failure("Existing Git storage cannot be safely copied"),
                        None,
                    ));
                }
                source = Some(mount.to_owned());
                format!(
                    "{name}_data_next_{}",
                    candidate.rsplit('_').next().unwrap_or("new")
                )
            } else {
                format!("{name}_data")
            };
            required!(self.require(
                vec!["volume".into(), "create".into(), volume.clone()],
                "Cannot create persistent CPU storage"
            ));
            args.extend([
                "-v".into(),
                format!("{volume}:/app"),
                git.image.clone(),
                "sh".into(),
                "-c".into(),
                git.script,
            ]);
            copy_image = Some(git.image);
            staged = Some(volume);
        } else {
            args.push(p.docker_image.clone().expect("validated image"));
        }
        // A missing image/create failure cannot interrupt the existing service.
        required!(self.require(args, "Cannot create CPU replacement"));
        let mut old_stopped = false;
        let mut old_renamed = false;
        let mut activated = false;
        let activate: Result<()> = async {
            if running {
                self.require(
                    vec!["stop".into(), name.into()],
                    "Cannot stop previous CPU service",
                )
                .await?;
                old_stopped = true;
            }
            if let Some(source) = source {
                self.require(
                    vec![
                        "run".into(),
                        "--rm".into(),
                        "--network".into(),
                        "none".into(),
                        "--cap-drop".into(),
                        "ALL".into(),
                        "--cap-add".into(),
                        "CHOWN".into(),
                        "--cap-add".into(),
                        "FOWNER".into(),
                        "--cap-add".into(),
                        "DAC_OVERRIDE".into(),
                        "-v".into(),
                        format!("{source}:/source:ro"),
                        "-v".into(),
                        format!("{}:/target", staged.as_ref().expect("staged storage")),
                        copy_image.expect("runtime image"),
                        "sh".into(),
                        "-c".into(),
                        "cp -a /source/. /target/".into(),
                    ],
                    "Cannot copy persistent project storage",
                )
                .await?;
            }
            if old {
                self.require(
                    vec!["rename".into(), name.into(), backup.into()],
                    "Cannot preserve previous CPU service",
                )
                .await?;
                old_renamed = true;
            }
            self.require(
                vec!["rename".into(), candidate.into(), name.into()],
                "Cannot activate CPU replacement",
            )
            .await?;
            activated = true;
            self.require(
                vec!["start".into(), name.into()],
                "Cannot start CPU replacement",
            )
            .await?;
            self.healthy(name, p.deploy_type == "git").await
        }
        .await;
        if let Err(error) = activate {
            let failed = if activated { name } else { candidate };
            let mut diagnostics = self.diagnostics(p, failed, error.0, false).await;
            let removed = self.remove_container(failed).await.is_ok();
            let renamed = !old_renamed
                || self
                    .require(
                        vec!["rename".into(), backup.into(), name.into()],
                        "Cannot restore previous CPU name",
                    )
                    .await
                    .is_ok();
            let restarted = !old_stopped
                || (renamed
                    && self
                        .require(
                            vec!["start".into(), name.into()],
                            "Cannot restart previous CPU service",
                        )
                        .await
                        .is_ok());
            diagnostics.rollback_restored = removed && renamed && restarted;
            return Err((error, Some(diagnostics)));
        }
        // Retain both old container and its /app volume for reviewed rollback/retention.
        Ok(())
    }
    pub async fn sync_tunnel(&self, p: &Project) -> Result<Option<String>> {
        p.validate()?;
        self.validate()?;
        let name = format!("cbtun_{}", &p.id[..8]);
        if p.access_scope != "public" {
            self.remove_container(&name).await?;
            return Ok(None);
        }
        let state = self
            .docker(vec![
                "inspect".into(),
                "--format".into(),
                "{{.State.Running}}|{{index .Config.Labels \"cbgpu.port\"}}".into(),
                name.clone(),
            ])
            .await?;
        if !state.success || state.stdout.trim() != format!("true|{}", p.host_port) {
            self.remove_container(&name).await?;
            self.require(
                vec![
                    "run".into(),
                    "-d".into(),
                    "--name".into(),
                    name.clone(),
                    "--network".into(),
                    "host".into(),
                    "--restart".into(),
                    "unless-stopped".into(),
                    "--cap-drop".into(),
                    "ALL".into(),
                    "--security-opt".into(),
                    "no-new-privileges:true".into(),
                    "--label".into(),
                    format!("cbgpu.port={}", p.host_port),
                    "--label".into(),
                    format!("cbgpu.project={}", p.id),
                    self.tunnel_image.clone(),
                    "tunnel".into(),
                    "--no-autoupdate".into(),
                    "--url".into(),
                    format!("http://127.0.0.1:{}", p.host_port),
                ],
                "Cannot start CPU public tunnel",
            )
            .await?;
        }
        let logs = self
            .require(
                vec!["logs".into(), "--tail".into(), "100".into(), name],
                "Cannot inspect CPU public tunnel",
            )
            .await?;
        Ok(tunnel_url(&format!("{}{}", logs.stdout, logs.stderr)))
    }
}
pub fn tunnel_url(logs: &str) -> Option<String> {
    logs.split(|c: char| !(c.is_ascii_alphanumeric() || ":/.-".contains(c)))
        .filter(|s| {
            s.starts_with("https://")
                && s.ends_with(".trycloudflare.com")
                && *s != "https://api.trycloudflare.com"
        })
        .rfind(|s| {
            let h = s
                .trim_start_matches("https://")
                .trim_end_matches(".trycloudflare.com");
            !h.is_empty()
                && h.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        })
        .map(String::from)
}
