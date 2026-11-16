use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure(pub &'static str);
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Failure {}
pub type Result<T> = std::result::Result<T, Failure>;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reservation {
    #[serde(default)]
    pub id: String,
    pub user_id: String,
    pub gpu_id: u8,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    #[serde(default = "approved")]
    pub status: String,
}
fn approved() -> String {
    "approved".into()
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredKey {
    pub user_id: String,
    pub public_key: String,
}
pub fn validate_public_key(value: &str) -> Result<String> {
    let fields: Vec<&str> = value.split_whitespace().collect();
    if fields.len() < 2 || fields[0] != "ssh-ed25519" {
        return Err(Failure("Only plain ssh-ed25519 public keys are supported"));
    }
    let raw = STANDARD
        .decode(fields[1])
        .map_err(|_| Failure("Invalid Ed25519 public key"))?;
    if raw.len() != 51
        || raw[..4] != [0, 0, 0, 11]
        || &raw[4..15] != b"ssh-ed25519"
        || raw[15..19] != [0, 0, 0, 32]
    {
        return Err(Failure("Invalid Ed25519 public key"));
    }
    Ok(format!("ssh-ed25519 {}", fields[1]))
}
pub fn allowed_from_rows(
    bookings: &[Reservation],
    keys: &[RegisteredKey],
    offered: &str,
    now: DateTime<Utc>,
) -> Result<BTreeMap<u8, DateTime<Utc>>> {
    let offered = validate_public_key(offered)?;
    let mut owners = BTreeMap::new();
    let mut ends = BTreeMap::new();
    let mut conflict = BTreeSet::new();
    for booking in bookings {
        if !(1..=4).contains(&booking.gpu_id)
            || booking.user_id.is_empty()
            || booking.status != "approved"
            || now < booking.start_time
            || now >= booking.end_time
        {
            continue;
        }
        if owners
            .get(&booking.gpu_id)
            .is_some_and(|owner| owner != &booking.user_id)
        {
            conflict.insert(booking.gpu_id);
        }
        owners.insert(booking.gpu_id, booking.user_id.clone());
        ends.entry(booking.gpu_id)
            .and_modify(|end: &mut DateTime<Utc>| *end = (*end).min(booking.end_time))
            .or_insert(booking.end_time);
    }
    let users: BTreeSet<&str> = keys
        .iter()
        .filter(|key| validate_public_key(&key.public_key).is_ok_and(|key| key == offered))
        .map(|key| key.user_id.as_str())
        .collect();
    Ok(owners
        .into_iter()
        .filter(|(gpu, owner)| !conflict.contains(gpu) && users.contains(owner.as_str()))
        .map(|(gpu, _)| (gpu, ends[&gpu]))
        .collect())
}
#[async_trait::async_trait]
pub trait Authorizer: Send + Sync {
    async fn allowed(&self, key: &str, now: DateTime<Utc>) -> Result<BTreeMap<u8, DateTime<Utc>>>;
}
#[derive(Clone)]
pub struct RestAuthorizer {
    base: reqwest::Url,
    secret: Arc<str>,
    client: reqwest::Client,
}
impl RestAuthorizer {
    pub fn new(url: &str, secret: String) -> Result<Self> {
        let base = reqwest::Url::parse(url).map_err(|_| Failure("Invalid backend URL"))?;
        if !["https", "http"].contains(&base.scheme())
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(Failure("Invalid backend URL"));
        }
        if secret.trim().is_empty() {
            return Err(Failure("Service credential unavailable"));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(6))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Failure("Backend client unavailable"))?;
        Ok(Self {
            base,
            secret: Arc::from(secret.trim()),
            client,
        })
    }
    async fn fetch<T: serde::de::DeserializeOwned>(
        &self,
        table: &str,
        query: &[(&str, String)],
    ) -> Result<T> {
        for attempt in 0..3 {
            let url = reqwest::Url::parse(&format!(
                "{}/rest/v1/{table}",
                self.base.as_str().trim_end_matches('/')
            ))
            .map_err(|_| Failure("Invalid backend URL"))?;
            let response = self
                .client
                .get(url)
                .query(query)
                .header("apikey", self.secret.as_ref())
                .bearer_auth(self.secret.as_ref())
                .send()
                .await;
            match response {
                Ok(mut response) if response.status().is_success() => {
                    if response.content_length().is_some_and(|size| size > 1 << 20) {
                        return Err(Failure("Backend response exceeds allowed size"));
                    }
                    let mut bytes = Vec::new();
                    while let Some(chunk) = response
                        .chunk()
                        .await
                        .map_err(|_| Failure("Backend response unavailable"))?
                    {
                        if bytes.len() + chunk.len() > 1 << 20 {
                            return Err(Failure("Backend response exceeds allowed size"));
                        }
                        bytes.extend_from_slice(&chunk);
                    }
                    return serde_json::from_slice(&bytes)
                        .map_err(|_| Failure("Invalid backend response"));
                }
                Ok(response)
                    if response.status().as_u16() != 429
                        && !response.status().is_server_error() =>
                {
                    return Err(Failure("Backend authorization denied"));
                }
                _ => {
                    if attempt == 2 {
                        return Err(Failure("Reservation lookup unavailable"));
                    }
                    tokio::time::sleep(Duration::from_millis((attempt + 1) * 200)).await;
                }
            }
        }
        Err(Failure("Reservation lookup unavailable"))
    }
}
impl RestAuthorizer {
    /// Complete approved booking history is paged; partial/default-1000-row responses must not reset active users.
    pub async fn snapshot(&self) -> Result<crate::controller::Snapshot> {
        #[derive(Deserialize)]
        struct Gpu {
            id: u8,
            password: Option<String>,
        }
        let gpus: Vec<Gpu> = self
            .fetch("gpus", &[("select", "id,password".into())])
            .await?;
        let mut reservations = Vec::new();
        let mut complete = false;
        for page in 0..100 {
            let rows: Vec<Reservation> = self
                .fetch(
                    "reservations",
                    &[
                        (
                            "select",
                            "id,gpu_id,user_id,start_time,end_time,status".into(),
                        ),
                        ("status", "eq.approved".into()),
                        ("order", "id".into()),
                        ("limit", "1000".into()),
                        ("offset", (page * 1000).to_string()),
                    ],
                )
                .await?;
            let done = rows.len() < 1000;
            reservations.extend(rows);
            if done {
                complete = true;
                break;
            }
        }
        if !complete {
            return Err(Failure(
                "Reservation history exceeded its bounded page limit",
            ));
        }
        if std::env::var("NEXT_PUBLIC_CBGPU_ACCOUNT_V2").as_deref() == Ok("true") {
            let active_owners: BTreeSet<String> = reservations
                .iter()
                .filter(|r| r.start_time <= Utc::now() && Utc::now() < r.end_time)
                .map(|r| r.user_id.clone())
                .collect();
            let mut denied = BTreeSet::new();
            for owner in active_owners {
                if !self.resource_allowed(&owner, "gpu").await? {
                    denied.insert(owner);
                }
            }
            reservations.retain(|r| !denied.contains(&r.user_id));
        }
        let users: Vec<String> = reservations
            .iter()
            .filter(|r| r.start_time <= Utc::now() && Utc::now() < r.end_time)
            .map(|r| r.user_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let keys = if users.is_empty() {
            Some(BTreeMap::new())
        } else {
            match self
                .fetch::<Vec<RegisteredKey>>(
                    "gpu_ssh_keys",
                    &[
                        ("select", "user_id,public_key".into()),
                        ("user_id", format!("in.({})", users.join(","))),
                    ],
                )
                .await
            {
                Ok(rows) => Some(
                    rows.into_iter()
                        .filter_map(|row| {
                            validate_public_key(&row.public_key)
                                .ok()
                                .map(|key| (row.user_id, key))
                        })
                        .collect(),
                ),
                Err(_) => None,
            }
        };
        Ok(crate::controller::Snapshot {
            passwords: gpus
                .into_iter()
                .filter_map(|gpu| gpu.password.map(|password| (gpu.id, password)))
                .collect(),
            reservations,
            keys,
        })
    }
}

#[async_trait::async_trait]
impl Authorizer for RestAuthorizer {
    async fn allowed(&self, key: &str, at: DateTime<Utc>) -> Result<BTreeMap<u8, DateTime<Utc>>> {
        tokio::time::timeout(Duration::from_secs(22), async {
            let bookings: Vec<Reservation> = self
                .fetch(
                    "reservations",
                    &[
                        ("select", "user_id,gpu_id,start_time,end_time".into()),
                        ("status", "eq.approved".into()),
                        ("start_time", format!("lte.{}", at.to_rfc3339())),
                        ("end_time", format!("gt.{}", at.to_rfc3339())),
                    ],
                )
                .await?;
            if bookings.is_empty() {
                return Ok(BTreeMap::new());
            }
            let mut bookings = bookings;
            let owners: BTreeSet<String> = bookings.iter().map(|r| r.user_id.clone()).collect();
            if std::env::var("NEXT_PUBLIC_CBGPU_ACCOUNT_V2").as_deref() == Ok("true") {
                for owner in &owners {
                    if !self.resource_allowed(owner, "gpu").await? {
                        bookings.retain(|r| r.user_id != *owner);
                    }
                }
            }
            let keys: Vec<RegisteredKey> = self
                .fetch(
                    "gpu_ssh_keys",
                    &[
                        ("select", "user_id,public_key".into()),
                        (
                            "user_id",
                            format!("in.({})", owners.into_iter().collect::<Vec<_>>().join(",")),
                        ),
                    ],
                )
                .await?;
            allowed_from_rows(&bookings, &keys, key, at)
        })
        .await
        .map_err(|_| Failure("Reservation authorization timed out"))?
    }
}

impl RestAuthorizer {
    pub async fn cpu_projects(&self) -> Result<Vec<crate::cpu::Project>> {
        let mut projects = Vec::new();
        for page in 0..100 {
            let rows: Vec<crate::cpu::Project> = self
                .fetch(
                    "cpu_projects",
                    &[
                        ("select", "*".into()),
                        ("status", "in.(approved,deleted)".into()),
                        ("order", "id".into()),
                        ("limit", "1000".into()),
                        ("offset", (page * 1000).to_string()),
                    ],
                )
                .await?;
            let done = rows.len() < 1000;
            projects.extend(rows.into_iter().filter(|p| {
                p.env_vars
                    .get("CBGPU_DEPLOY_ENGINE")
                    .is_none_or(|v| v != "compose")
            }));
            if done {
                return Ok(projects);
            }
        }
        Err(Failure("CPU project listing is incomplete"))
    }
    pub async fn cpu_current(&self, id: &str) -> Result<Option<crate::cpu::Project>> {
        crate::cpu::container_name(id)?;
        let rows: Vec<crate::cpu::Project> = self
            .fetch(
                "cpu_projects",
                &[("select", "*".into()), ("id", format!("eq.{id}"))],
            )
            .await?;
        Ok(rows.into_iter().next())
    }
    pub async fn cpu_patch(&self, id: &str, value: serde_json::Value) -> Result<()> {
        if !self.cpu_patch_rows(id, value, None).await? {
            return Err(Failure(
                "CPU state synchronization did not update one project",
            ));
        }
        Ok(())
    }
    /// Do not acknowledge a later redeploy request or update a deleted project's URL.
    pub async fn cpu_patch_current(
        &self,
        project: &crate::cpu::Project,
        value: serde_json::Value,
    ) -> Result<bool> {
        self.cpu_patch_rows(&project.id, value, Some(project)).await
    }
    async fn cpu_patch_rows(
        &self,
        id: &str,
        value: serde_json::Value,
        expected: Option<&crate::cpu::Project>,
    ) -> Result<bool> {
        crate::cpu::container_name(id)?;
        let url = format!(
            "{}/rest/v1/cpu_projects",
            self.base.as_str().trim_end_matches('/')
        );
        let mut query = vec![("id", format!("eq.{id}")), ("select", "id".into())];
        if let Some(project) = expected {
            query.push(("status", "eq.approved".into()));
            query.push((
                "deploy_requested_at",
                project
                    .deploy_requested_at
                    .map(|time| format!("eq.{}", time.to_rfc3339()))
                    .unwrap_or_else(|| "is.null".into()),
            ));
        }
        let mut response = self
            .client
            .patch(url)
            .query(&query)
            .header("apikey", self.secret.as_ref())
            .bearer_auth(self.secret.as_ref())
            .header("Prefer", "return=representation")
            .json(&value)
            .send()
            .await
            .map_err(|_| Failure("CPU state synchronization unavailable"))?;
        if !response.status().is_success()
            || response.content_length().is_some_and(|size| size > 65536)
        {
            return Err(Failure("CPU state synchronization rejected"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure("CPU state synchronization unavailable"))?
        {
            if bytes.len() + chunk.len() > 65536 {
                return Err(Failure("CPU state synchronization response too large"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&bytes)
            .map_err(|_| Failure("Invalid CPU state synchronization result"))?;
        if rows.len() > 1 {
            return Err(Failure(
                "CPU state synchronization affected multiple projects",
            ));
        }
        Ok(rows.len() == 1)
    }
}

impl RestAuthorizer {
    pub async fn resource_allowed(&self, owner: &str, resource: &str) -> Result<bool> {
        if owner.is_empty() {
            return Ok(false);
        }
        let url = format!(
            "{}/rest/v1/rpc/cbgpu_service_can_use_resource",
            self.base.as_str().trim_end_matches('/')
        );
        let response = self
            .client
            .post(url)
            .header("apikey", self.secret.as_ref())
            .bearer_auth(self.secret.as_ref())
            .json(&serde_json::json!({"user_id":owner,"resource":resource,"target_school":"cbsh"}))
            .send()
            .await
            .map_err(|_| Failure("Fresh resource permission unavailable"))?;
        if !response.status().is_success() {
            return Err(Failure("Fresh resource permission unavailable"));
        }
        response
            .json::<bool>()
            .await
            .map_err(|_| Failure("Invalid resource permission response"))
    }
}
