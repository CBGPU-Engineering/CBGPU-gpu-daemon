//! Bounded native process/Docker adapter. Arguments are never interpreted by a host shell.
//! Reset stages the replacement first and preserves/restores the previous container and all volumes.
use crate::{
    authorization::{Failure, Result},
    controller::{Config, Driver, GpuConfig},
};
use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
use rand::{RngCore, rngs::OsRng};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
#[derive(Clone, Default)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}
#[async_trait::async_trait]
pub trait Runner: Send + Sync {
    async fn run(&self, program: &str, args: &[String], input: Option<Vec<u8>>) -> Result<Output>;
}
async fn read_tail(mut stream: impl AsyncRead + Unpin) -> Result<String> {
    const LIMIT: usize = 65536;
    let mut buffer = [0; 8192];
    let mut tail = Vec::with_capacity(LIMIT);
    let mut truncated = false;
    loop {
        let size = stream
            .read(&mut buffer)
            .await
            .map_err(|_| Failure("Command output failed"))?;
        if size == 0 {
            break;
        }
        let overflow = (tail.len() + size).saturating_sub(LIMIT);
        if overflow > 0 {
            tail.drain(..overflow);
            truncated = true;
        }
        tail.extend_from_slice(&buffer[..size]);
    }
    let prefix = if truncated {
        "[Earlier output truncated]\n"
    } else {
        ""
    };
    Ok(format!("{prefix}{}", String::from_utf8_lossy(&tail)))
}
pub struct NativeRunner;
#[async_trait::async_trait]
impl Runner for NativeRunner {
    async fn run(&self, program: &str, args: &[String], input: Option<Vec<u8>>) -> Result<Output> {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .kill_on_drop(true)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|_| Failure("Infrastructure command unavailable"))?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or(Failure("Command output unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or(Failure("Command output unavailable"))?;
        let result = tokio::time::timeout(Duration::from_secs(120), async {
            let write_input = async {
                if let Some(bytes) = input {
                    let mut stdin = stdin.ok_or(Failure("Command input unavailable"))?;
                    stdin
                        .write_all(&bytes)
                        .await
                        .map_err(|_| Failure("Command input failed"))?;
                    stdin
                        .shutdown()
                        .await
                        .map_err(|_| Failure("Command input failed"))?;
                }
                Ok::<(), Failure>(())
            };
            // Always drain both pipes, even after the retained tail reaches its cap.
            // Stopping reads at the cap can deadlock a verbose child on its pipe buffer.
            let (out, err, written, status) = tokio::join!(
                read_tail(stdout),
                read_tail(stderr),
                write_input,
                child.wait()
            );
            written?;
            let status = status.map_err(|_| Failure("Infrastructure command failed"))?;
            Ok(Output {
                success: status.success(),
                stdout: out?,
                stderr: err?,
            })
        })
        .await;
        match result {
            Ok(value) => value,
            Err(_) => {
                let _ = child.kill().await;
                Err(Failure("Infrastructure command timed out"))
            }
        }
    }
}
#[derive(Clone)]
pub struct Docker {
    pub config: Config,
    pub private_address: std::net::Ipv4Addr,
    pub runner: Arc<dyn Runner>,
    pub verification_delay: Duration,
}
impl Docker {
    pub fn new(
        config: Config,
        private_address: std::net::Ipv4Addr,
        runner: Arc<dyn Runner>,
    ) -> Result<Self> {
        config.validate()?;
        let octets = private_address.octets();
        let tailnet = octets[0] == 100 && (64..=127).contains(&octets[1]);
        if private_address.is_unspecified()
            || (!private_address.is_private() && !private_address.is_loopback() && !tailnet)
        {
            return Err(Failure("Invalid private GPU bind address"));
        }
        Ok(Self {
            config,
            private_address,
            runner,
            verification_delay: Duration::from_secs(2),
        })
    }
    async fn command(&self, args: Vec<String>) -> Result<Output> {
        self.runner.run("docker", &args, None).await
    }
    async fn require(&self, args: Vec<String>, message: &'static str) -> Result<Output> {
        let output = self.command(args).await?;
        if output.success {
            Ok(output)
        } else {
            Err(Failure(message))
        }
    }
    async fn copy(&self, container: &str, destination: &str, content: &[u8]) -> Result<()> {
        let directory =
            tempfile::tempdir().map_err(|_| Failure("Private staging directory unavailable"))?;
        let path = directory.path().join("managed-config");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        let mut file = options
            .open(&path)
            .map_err(|_| Failure("Private staging file unavailable"))?;
        file.write_all(content)
            .map_err(|_| Failure("Private staging write failed"))?;
        file.sync_all()
            .map_err(|_| Failure("Private staging write failed"))?;
        self.require(
            vec![
                "cp".into(),
                path.to_string_lossy().into(),
                format!("{container}:{destination}"),
            ],
            "Container configuration copy failed",
        )
        .await?;
        self.require(
            vec![
                "exec".into(),
                container.into(),
                "chmod".into(),
                "600".into(),
                destination.into(),
            ],
            "Container configuration permissions failed",
        )
        .await?;
        Ok(())
    }
    fn host_key_directory(&self, id: u8) -> PathBuf {
        Path::new(&self.config.docker.ssh_host_keys_dir).join(id.to_string())
    }
    async fn ensure_host_key(&self, id: u8) -> Result<()> {
        let directory = self.host_key_directory(id);
        std::fs::create_dir_all(&directory)
            .map_err(|_| Failure("GPU SSH host key directory unavailable"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| Failure("GPU SSH host key permissions failed"))?;
        }
        let key = directory.join("ssh_host_ed25519_key");
        if !key.exists() {
            let output = self
                .runner
                .run(
                    "ssh-keygen",
                    &[
                        "-q".into(),
                        "-t".into(),
                        "ed25519".into(),
                        "-N".into(),
                        String::new(),
                        "-f".into(),
                        key.to_string_lossy().into(),
                    ],
                    None,
                )
                .await?;
            if !output.success {
                return Err(Failure("GPU SSH host key generation failed"));
            }
        }
        let metadata =
            std::fs::symlink_metadata(&key).map_err(|_| Failure("GPU SSH host key unavailable"))?;
        if !metadata.is_file() {
            return Err(Failure("Invalid GPU SSH host key"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))
                .map_err(|_| Failure("GPU SSH host key permissions failed"))?;
        }
        Ok(())
    }
    pub fn reset_arguments(&self, id: u8, gpu: &GpuConfig, candidate: &str) -> Vec<String> {
        let mut args = vec![
            "create".into(),
            "--name".into(),
            candidate.into(),
            "--gpus".into(),
            format!("device={}", gpu.device),
            "--shm-size".into(),
            "32g".into(),
            "-e".into(),
            "JUPYTER_SKIP_TRASH=yes".into(),
            "--restart".into(),
            "always".into(),
        ];
        let mut addresses = Vec::new();
        for address in [
            "127.0.0.1".to_owned(),
            self.config.docker.school_bind_address.clone(),
            self.private_address.to_string(),
        ] {
            if !addresses.contains(&address) {
                addresses.push(address)
            }
        }
        for address in addresses {
            args.extend([
                "-p".into(),
                format!(
                    "{address}:{}:{}",
                    gpu.host_port, self.config.docker.container_port
                ),
            ])
        }
        if let Some(port) = gpu.ssh_port {
            args.extend([
                "-p".into(),
                format!("{}:{port}:22", self.private_address),
                "-v".into(),
                format!(
                    "{}:/etc/ssh/hostkeys:ro",
                    self.host_key_directory(id).display()
                ),
            ])
        }
        args.push(self.config.docker.image.clone());
        args
    }
}
#[async_trait::async_trait]
impl Driver for Docker {
    async fn has_password_config(&self, gpu: &GpuConfig) -> Result<bool> {
        let output = self
            .command(vec![
                "exec".into(),
                gpu.container.clone(),
                "sh".into(),
                "-c".into(),
                "if test -s /root/.jupyter/jupyter_server_config.py; then printf configured; else printf absent; fi".into(),
            ])
            .await?;
        if !output.success {
            return Err(Failure("Cannot inspect running GPU authentication"));
        }
        match output.stdout.as_str() {
            "configured" => Ok(true),
            "absent" => Ok(false),
            _ => Err(Failure("Unexpected GPU authentication inspection response")),
        }
    }
    async fn write_keys(&self, gpu: &GpuConfig, keys: &[String]) -> Result<()> {
        let mut normalized = Vec::new();
        for key in keys {
            normalized.push(crate::authorization::validate_public_key(key)?)
        }
        self.require(
            vec![
                "exec".into(),
                "--user".into(),
                "0:0".into(),
                gpu.container.clone(),
                "mkdir".into(),
                "-p".into(),
                "/root/.ssh".into(),
            ],
            "Container SSH directory unavailable",
        )
        .await?;
        self.require(
            vec![
                "exec".into(),
                "--user".into(),
                "0:0".into(),
                gpu.container.clone(),
                "chmod".into(),
                "700".into(),
                "/root/.ssh".into(),
            ],
            "Container SSH directory permissions failed",
        )
        .await?;
        let contents = if normalized.is_empty() {
            String::new()
        } else {
            normalized.join("\n") + "\n"
        };
        self.copy(
            &gpu.container,
            "/root/.ssh/authorized_keys",
            contents.as_bytes(),
        )
        .await?;
        // docker cp can retain the unprivileged controller's UID. sshd StrictModes
        // rejects root's authorized_keys unless both managed paths belong to root.
        self.require(
            vec![
                "exec".into(),
                "--user".into(),
                "0:0".into(),
                gpu.container.clone(),
                "chown".into(),
                "0:0".into(),
                "/root/.ssh".into(),
                "/root/.ssh/authorized_keys".into(),
            ],
            "Container SSH ownership repair failed",
        )
        .await?;
        Ok(())
    }
    async fn password(&self, gpu: &GpuConfig, password: &str) -> Result<()> {
        let mut salt = [0; 16];
        OsRng
            .try_fill_bytes(&mut salt)
            .map_err(|_| Failure("Secure randomness unavailable"))?;
        let salt =
            SaltString::encode_b64(&salt).map_err(|_| Failure("Password salt unavailable"))?;
        let hashed = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|_| Failure("Jupyter password hashing failed"))?
            .to_string();
        let hash = format!("argon2:{hashed}");
        let content = format!(
            "c.ServerApp.password = '{hash}'\nc.NotebookApp.password = '{hash}'\nc.ServerApp.token = ''\nc.NotebookApp.token = ''\n"
        );
        self.require(
            vec![
                "exec".into(),
                gpu.container.clone(),
                "mkdir".into(),
                "-p".into(),
                "/root/.jupyter".into(),
            ],
            "Jupyter configuration directory unavailable",
        )
        .await?;
        for name in ["jupyter_server_config.py", "jupyter_notebook_config.py"] {
            self.copy(
                &gpu.container,
                &format!("/root/.jupyter/{name}"),
                content.as_bytes(),
            )
            .await?;
        }
        self.require(
            vec!["restart".into(), gpu.container.clone()],
            "Jupyter authentication restart failed",
        )
        .await?;
        Ok(())
    }
    async fn reset(&self, id: u8, gpu: &GpuConfig) -> Result<()> {
        if gpu.ssh_port.is_some() {
            self.ensure_host_key(id).await?;
        }
        self.require(
            vec![
                "image".into(),
                "inspect".into(),
                self.config.docker.image.clone(),
            ],
            "GPU base image unavailable",
        )
        .await?;
        let mut random = [0; 4];
        OsRng
            .try_fill_bytes(&mut random)
            .map_err(|_| Failure("Secure randomness unavailable"))?;
        let suffix = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let candidate = format!("{}_next_{suffix}", gpu.container);
        let backup = format!("{}_previous_{suffix}", gpu.container);
        self.require(
            self.reset_arguments(id, gpu, &candidate),
            "GPU replacement creation failed; existing container untouched",
        )
        .await?;
        let mut old_renamed = false;
        let mut candidate_renamed = false;
        let mut old_stopped = false;
        let result: Result<()> = async {
            let old = self
                .command(vec![
                    "inspect".into(),
                    "--format".into(),
                    "{{.State.Running}}".into(),
                    gpu.container.clone(),
                ])
                .await?;
            if !old.success
                && !old.stderr.to_lowercase().contains("no such object")
                && !old.stderr.to_lowercase().contains("no such container")
            {
                return Err(Failure("Cannot inspect existing GPU container"));
            }
            if old.success {
                if old.stdout.trim() == "true" {
                    self.require(
                        vec!["stop".into(), gpu.container.clone()],
                        "Cannot stop previous GPU session",
                    )
                    .await?;
                    old_stopped = true;
                }
                self.require(
                    vec!["rename".into(), gpu.container.clone(), backup.clone()],
                    "Cannot preserve previous GPU session",
                )
                .await?;
                old_renamed = true;
            }
            self.require(
                vec!["rename".into(), candidate.clone(), gpu.container.clone()],
                "Cannot activate GPU replacement",
            )
            .await?;
            candidate_renamed = true;
            self.require(
                vec!["start".into(), gpu.container.clone()],
                "Cannot start GPU replacement",
            )
            .await?;
            tokio::time::sleep(self.verification_delay).await;
            let running = self
                .require(
                    vec![
                        "inspect".into(),
                        "--format".into(),
                        "{{.State.Running}}".into(),
                        gpu.container.clone(),
                    ],
                    "Cannot verify GPU replacement",
                )
                .await?;
            if running.stdout.trim() != "true" {
                return Err(Failure("GPU replacement exited after startup"));
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let failed = if candidate_renamed {
                gpu.container.clone()
            } else {
                candidate
            };
            let _ = self.command(vec!["rm".into(), "-f".into(), failed]).await;
            if old_renamed {
                self.require(vec!["rename".into(),backup,gpu.container.clone()],"GPU reset failed and previous container restoration requires operator intervention").await?;
            }
            if old_stopped {
                self.require(vec!["start".into(),gpu.container.clone()],"GPU reset failed and previous container restart requires operator intervention").await?;
            }
            return Err(error);
        }
        // Preserve the previous container and its filesystem for a separately reviewed retention cleanup.
        // Neither success nor rollback ever issues docker volume rm or docker rm -v.
        Ok(())
    }
}
