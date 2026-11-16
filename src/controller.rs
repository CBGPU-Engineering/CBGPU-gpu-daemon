//! Native reservation reconciliation. A successful backend poll is required before new access is granted.
//! Cached known deadlines are enforced even while the database is unavailable.
use crate::authorization::{Failure, Reservation, Result, validate_public_key};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Serialize, Deserialize)]
pub struct GpuConfig {
    pub username: String,
    pub container: String,
    pub device: String,
    pub host_port: u16,
    pub ssh_port: Option<u16>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct DockerConfig {
    pub image: String,
    #[serde(default = "tunnel_image")]
    pub tunnel_image: String,
    pub container_port: u16,
    pub school_bind_address: String,
    #[serde(default = "host_keys")]
    pub ssh_host_keys_dir: String,
}
fn tunnel_image() -> String {
    "cloudflare/cloudflared:latest".into()
}
fn host_keys() -> String {
    "/home/cbgpu/.local/share/cbgpu/gpu-ssh".into()
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    pub supabase_url: String,
    pub supabase_anon_key: String,
    pub gpus: BTreeMap<u8, GpuConfig>,
    pub docker: DockerConfig,
    #[serde(default = "poll")]
    pub poll_interval_seconds: u64,
    #[serde(default)]
    pub initialize_idle_containers: bool,
}
fn poll() -> u64 {
    30
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.supabase_url.is_empty()
            || self.supabase_anon_key.is_empty()
            || self.gpus.is_empty()
            || self.docker.image.is_empty()
            || self.docker.image.starts_with('-')
            || !self
                .docker
                .image
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._:@-".contains(&b))
            || self.docker.container_port == 0
            || !(1..=300).contains(&self.poll_interval_seconds)
        {
            return Err(Failure("Invalid controller configuration"));
        }
        let school: std::net::Ipv4Addr = self
            .docker
            .school_bind_address
            .parse()
            .map_err(|_| Failure("School bind address must be a private IPv4 address"))?;
        if !school.is_private() || school.is_loopback() || school.is_unspecified() {
            return Err(Failure(
                "School bind address must be a private IPv4 address",
            ));
        }
        let mut containers = BTreeSet::new();
        let mut devices = BTreeSet::new();
        let mut ports = BTreeSet::new();
        for (id, gpu) in &self.gpus {
            if !(1..=4).contains(id)
                || gpu.username.is_empty()
                || gpu.container.is_empty()
                || gpu.container.len() > 120
                || !gpu
                    .container
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                || gpu.container.starts_with('-')
                || gpu.device.is_empty()
                || gpu.device.chars().any(char::is_whitespace)
                || gpu.host_port == 0
                || !containers.insert(&gpu.container)
                || !devices.insert(&gpu.device)
                || !ports.insert(gpu.host_port)
                || gpu
                    .ssh_port
                    .is_some_and(|port| port == 0 || !ports.insert(port))
            {
                return Err(Failure("Invalid or colliding GPU configuration"));
            }
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct Snapshot {
    pub passwords: BTreeMap<u8, String>,
    pub reservations: Vec<Reservation>,
    pub keys: Option<BTreeMap<String, String>>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct ActiveSession {
    pub ids: BTreeSet<String>,
    pub deadline: DateTime<Utc>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub active: BTreeMap<u8, ActiveSession>,
    pub password_digests: BTreeMap<u8, String>,
    pub applied_keys: BTreeMap<u8, Vec<String>>,
    pub reset_done: BTreeSet<String>,
    #[serde(default)]
    pub initialized: bool,
}
#[derive(Default, Serialize)]
pub struct Report {
    pub backend_available: bool,
    pub active_gpus: Vec<u8>,
    pub actions: Vec<String>,
    pub errors: Vec<String>,
    pub cpu_controller_managed_by_this_process: bool,
}
#[async_trait::async_trait]
pub trait Driver: Send + Sync {
    async fn has_password_config(&self, gpu: &GpuConfig) -> Result<bool>;
    async fn reset(&self, id: u8, gpu: &GpuConfig) -> Result<()>;
    async fn write_keys(&self, gpu: &GpuConfig, keys: &[String]) -> Result<()>;
    async fn password(&self, gpu: &GpuConfig, password: &str) -> Result<()>;
}
fn password_digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
impl State {
    async fn reset(
        &mut self,
        id: u8,
        cfg: &Config,
        driver: &dyn Driver,
        report: &mut Report,
    ) -> bool {
        let Some(gpu) = cfg.gpus.get(&id) else {
            return false;
        };
        match driver.reset(id, gpu).await {
            Ok(()) => {
                if let Some(previous) = self.active.remove(&id) {
                    self.reset_done.extend(previous.ids)
                }
                self.password_digests.remove(&id);
                self.applied_keys.remove(&id);
                report.actions.push(format!("gpu{id}:reset"));
                true
            }
            Err(_) => {
                report.errors.push(format!(
                    "gpu{id}:reset failed; existing container retained or restored"
                ));
                false
            }
        }
    }
    pub async fn reconcile(
        &mut self,
        cfg: &Config,
        snapshot: Option<Snapshot>,
        at: DateTime<Utc>,
        driver: &dyn Driver,
    ) -> Report {
        let mut report = Report {
            backend_available: snapshot.is_some(),
            ..Default::default()
        };
        let Some(snapshot) = snapshot else {
            let expired: Vec<u8> = self
                .active
                .iter()
                .filter(|(_, session)| at >= session.deadline)
                .map(|(gpu, _)| *gpu)
                .collect();
            for id in expired {
                self.reset(id, cfg, driver, &mut report).await;
            }
            report.active_gpus = self.active.keys().copied().collect();
            return report;
        };
        let mut groups: BTreeMap<u8, Vec<&Reservation>> = BTreeMap::new();
        for booking in &snapshot.reservations {
            if booking.status == "approved"
                && booking.start_time <= at
                && at < booking.end_time
                && cfg.gpus.contains_key(&booking.gpu_id)
            {
                groups.entry(booking.gpu_id).or_default().push(booking)
            }
        }
        let mut blocked = BTreeSet::new();
        if !self.initialized {
            self.reset_done.extend(
                snapshot
                    .reservations
                    .iter()
                    .filter(|r| r.end_time < at)
                    .map(|r| r.id.clone()),
            );
            for (id, gpu) in &cfg.gpus {
                if groups.contains_key(id) {
                    if !self.password_digests.contains_key(id) {
                        match driver.has_password_config(gpu).await {
                            Ok(true) => {
                                if let Some(password) =
                                    snapshot.passwords.get(id).filter(|v| !v.is_empty())
                                {
                                    self.password_digests.insert(*id, password_digest(password));
                                }
                            }
                            Ok(false) => {}
                            Err(_) => {
                                report.errors.push(format!("gpu{id}:cannot inspect running authentication; notebook left intact"));
                                blocked.insert(*id);
                            }
                        }
                    }
                } else if cfg.initialize_idle_containers {
                    self.reset(*id, cfg, driver, &mut report).await;
                }
            }
            self.initialized = blocked.is_empty();
            for id in &blocked {
                if !self.active.contains_key(id)
                    && let Some(group) = groups.get(id)
                {
                    self.active.insert(
                        *id,
                        ActiveSession {
                            ids: group.iter().map(|r| r.id.clone()).collect(),
                            deadline: group
                                .iter()
                                .map(|r| r.end_time)
                                .max()
                                .expect("nonempty group"),
                        },
                    );
                }
            }
        }
        let previous: Vec<u8> = self.active.keys().copied().collect();
        for id in previous {
            if blocked.contains(&id) {
                continue;
            }
            let ids: BTreeSet<String> = groups
                .get(&id)
                .into_iter()
                .flatten()
                .map(|r| r.id.clone())
                .collect();
            if self.active[&id].ids != ids && !self.reset(id, cfg, driver, &mut report).await {
                blocked.insert(id);
            }
        }
        for (id, group) in &groups {
            if blocked.contains(id) {
                continue;
            }
            let gpu = &cfg.gpus[id];
            let owners: BTreeSet<&str> = group.iter().map(|r| r.user_id.as_str()).collect();
            self.active.insert(
                *id,
                ActiveSession {
                    ids: group.iter().map(|r| r.id.clone()).collect(),
                    deadline: group
                        .iter()
                        .map(|r| r.end_time)
                        .max()
                        .expect("nonempty group"),
                },
            );
            let keys: Vec<String> = if owners.len() == 1 {
                owners
                    .iter()
                    .filter_map(|owner| snapshot.keys.as_ref()?.get(*owner))
                    .filter_map(|key| validate_public_key(key).ok())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            } else {
                report.errors.push(format!(
                    "gpu{id}:overlapping owners; access provisioning denied"
                ));
                Vec::new()
            };
            if self.applied_keys.get(id) != Some(&keys) {
                match driver.write_keys(gpu, &keys).await {
                    Ok(()) => {
                        self.applied_keys.insert(*id, keys);
                        report
                            .actions
                            .push(format!("gpu{id}:SSH keys synchronized"));
                    }
                    Err(_) => report
                        .errors
                        .push(format!("gpu{id}:SSH key synchronization failed")),
                }
            }
            if owners.len() == 1
                && let Some(password) = snapshot.passwords.get(id).filter(|v| !v.is_empty())
            {
                let digest = password_digest(password);
                if self.password_digests.get(id) != Some(&digest) {
                    match driver.password(gpu, password).await {
                        Ok(()) => {
                            self.password_digests.insert(*id, digest);
                            report
                                .actions
                                .push(format!("gpu{id}:authentication synchronized"));
                        }
                        Err(_) => report
                            .errors
                            .push(format!("gpu{id}:authentication synchronization failed")),
                    }
                }
            }
        }
        let ended: BTreeSet<u8> = snapshot
            .reservations
            .iter()
            .filter(|r| {
                r.status == "approved"
                    && r.end_time < at
                    && !self.reset_done.contains(&r.id)
                    && !groups.contains_key(&r.gpu_id)
            })
            .map(|r| r.gpu_id)
            .collect();
        for id in ended {
            if self.reset(id, cfg, driver, &mut report).await {
                self.reset_done.extend(
                    snapshot
                        .reservations
                        .iter()
                        .filter(|r| r.gpu_id == id && r.end_time < at)
                        .map(|r| r.id.clone()),
                );
            }
        }
        report.active_gpus = self.active.keys().copied().collect();
        report
    }
}
