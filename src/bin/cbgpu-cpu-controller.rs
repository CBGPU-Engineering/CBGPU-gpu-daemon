//! Independent CPU event loop: installation cannot postpone GPU booking expiry.
use cbgpu_gpu_daemon::{
    authorization::{Failure, RestAuthorizer},
    controller::Config,
    cpu::{DeploymentFailure, Engine, Project, collision_prefixes, scheduling_order},
    docker::NativeRunner,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::task::JoinSet;
fn secret(path: &Path) -> Result<String, Failure> {
    use std::os::unix::fs::PermissionsExt;
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| Failure("Service credential unavailable"))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(Failure("Service credential must be a private regular file"));
    }
    std::fs::read_to_string(path).map_err(|_| Failure("Service credential unavailable"))
}
fn store(directory: &Path, id: &str, value: &impl serde::Serialize) -> Result<(), Failure> {
    use std::io::Write;
    cbgpu_gpu_daemon::cpu::container_name(id)?;
    let mut f = tempfile::NamedTempFile::new_in(directory)
        .map_err(|_| Failure("CPU diagnostics storage unavailable"))?;
    serde_json::to_writer(&mut f, value).map_err(|_| Failure("Cannot encode CPU diagnostics"))?;
    f.flush()
        .map_err(|_| Failure("Cannot store CPU diagnostics"))?;
    f.as_file()
        .sync_all()
        .map_err(|_| Failure("Cannot store CPU diagnostics"))?;
    f.persist(directory.join(format!("{id}.json")))
        .map_err(|_| Failure("Cannot commit CPU diagnostics"))?;
    Ok(())
}
#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{e}");
        std::process::exit(1)
    }
}
async fn run() -> Result<(), Failure> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 4 || args[0] != "--run" || args[1] != "--apply" {
        println!(
            "Native CPU controller candidate. Explicit activation only: cbgpu-cpu-controller --run --apply CONFIG PRIVATE_STATE_DIRECTORY. Does not run GPU controller or Compose projects."
        );
        return Ok(());
    }
    let config: Config = serde_yaml_ng::from_str(
        &std::fs::read_to_string(&args[2]).map_err(|_| Failure("Configuration unavailable"))?,
    )
    .map_err(|_| Failure("Invalid CPU configuration"))?;
    config.validate()?;
    let directory = PathBuf::from(&args[3]);
    std::fs::create_dir_all(&directory)
        .map_err(|_| Failure("Cannot create CPU state directory"))?;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let metadata = std::fs::symlink_metadata(&directory)
        .map_err(|_| Failure("CPU state directory unavailable"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Failure("CPU state directory must be a real directory"));
    }
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| Failure("Cannot secure CPU state directory"))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(directory.join("worker.lock"))
        .map_err(|_| Failure("Cannot open CPU worker lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .map_err(|_| Failure("Another native CPU controller owns this state directory"))?;
    let path = std::env::var("CBGPU_SUPABASE_SERVICE_ROLE_KEY_FILE")
        .unwrap_or_else(|_| "/home/cbgpu/.config/cbgpu/service_role_key".into());
    let credential = if let Ok(value) = std::env::var("CBGPU_SUPABASE_SERVICE_ROLE_KEY") {
        value
    } else {
        secret(Path::new(&path))?
    };
    let api = RestAuthorizer::new(&config.supabase_url, credential)?;
    use cbgpu_gpu_daemon::docker::Runner;
    let runner = Arc::new(NativeRunner);
    let address = runner
        .run("tailscale", &["ip".into(), "-4".into()], None)
        .await
        .ok()
        .filter(|r| r.success)
        .and_then(|r| r.stdout.lines().next().and_then(|s| s.parse().ok()))
        .unwrap_or(std::net::Ipv4Addr::LOCALHOST);
    let engine = Engine {
        runner,
        school: config
            .docker
            .school_bind_address
            .parse()
            .map_err(|_| Failure("Invalid school bind"))?,
        private: address,
        tunnel_image: config.docker.tunnel_image.clone(),
        poll: Duration::from_secs(1),
        startup_timeout: Duration::from_secs(600),
    };
    engine.validate()?;
    let mut jobs: JoinSet<(Project, std::result::Result<(), DeploymentFailure>)> = JoinSet::new();
    let mut active = BTreeSet::new();
    let mut cleaned = BTreeSet::new();
    let mut last_scheduled: Option<String> = None;
    let mut retry: BTreeMap<String, (Option<chrono::DateTime<chrono::Utc>>, tokio::time::Instant)> =
        BTreeMap::new();
    loop {
        while let Some(result) = jobs.try_join_next() {
            match result {
                Ok((p, result)) => {
                    active.remove(&p.id);
                    match result {
                        Ok(()) => {
                            retry.remove(&p.id);
                            if p.status == "deleted" {
                                cleaned.insert(p.id);
                            }
                        }
                        Err(failure) => {
                            store(
                                &directory,
                                &p.id,
                                &serde_json::json!({"status":"failed","at":chrono::Utc::now(),"failure":failure}),
                            )?;
                            retry.insert(
                                p.id,
                                (
                                    p.deploy_requested_at,
                                    tokio::time::Instant::now() + Duration::from_secs(300),
                                ),
                            );
                        }
                    }
                }
                Err(_) => {
                    return Err(Failure(
                        "CPU task failed unexpectedly; inspect diagnostics before restarting",
                    ));
                }
            }
        }
        match api.cpu_projects().await {
            Ok(mut projects) => {
                let collisions = collision_prefixes(&projects);
                scheduling_order(&mut projects, last_scheduled.as_deref());
                for p in projects {
                    if active.len() >= 2 {
                        break;
                    }
                    if active.contains(&p.id)
                        || p.id.len() < 8
                        || collisions.contains(&p.id[..8])
                        || p.validate().is_err()
                    {
                        continue;
                    }
                    if p.status == "deleted" && cleaned.contains(&p.id) {
                        continue;
                    }
                    if retry.get(&p.id).is_some_and(|(request, time)| {
                        *request == p.deploy_requested_at && tokio::time::Instant::now() < *time
                    }) {
                        continue;
                    }
                    let needs = p.status == "deleted"
                        || p.deployed_at.is_none()
                        || p.deploy_requested_at.is_some_and(|request| {
                            p.deployed_at.is_none_or(|deployed| request > deployed)
                        })
                        || !engine.running(&p).await.unwrap_or(true);
                    if !needs && p.access_scope != "public" && p.public_url.is_none() {
                        continue;
                    }
                    last_scheduled = Some(p.id.clone());
                    active.insert(p.id.clone());
                    let api = api.clone();
                    let engine = engine.clone();
                    let state_directory = directory.clone();
                    jobs.spawn(async move{let result=process(&api,&engine,&p,needs).await;if result.is_ok(){let _=store(&state_directory,&p.id,&serde_json::json!({"status":if p.status=="deleted"{"removed"}else{"ready"},"at":chrono::Utc::now()}));}(p,result)});
                }
            }
            Err(_) => eprintln!("CPU project lookup unavailable; keeping existing services"),
        }
        tokio::select! {_=tokio::signal::ctrl_c()=>break,_=tokio::time::sleep(Duration::from_secs(config.poll_interval_seconds.max(1)))=>{}}
    }
    // Do not cancel a half-finished container swap on graceful shutdown.
    while let Some(result) = jobs.join_next().await {
        if let Ok((p, Err(failure))) = result {
            let _ = store(&directory, &p.id, &failure);
        }
    }
    drop(lock);
    Ok(())
}
async fn process(
    api: &RestAuthorizer,
    engine: &Engine,
    p: &Project,
    deploy: bool,
) -> Result<(), DeploymentFailure> {
    let failure = |e: Failure| DeploymentFailure {
        message: e.0.into(),
        logs: String::new(),
        health: String::new(),
        rollback_restored: true,
    };
    let current = api
        .cpu_current(&p.id)
        .await
        .map_err(failure)?
        .ok_or_else(|| failure(Failure("CPU project no longer exists")))?;
    // Re-read immediately before execution: stale poll snapshots cannot launch deleted/revised projects.
    if current.status != p.status || current.deploy_requested_at != p.deploy_requested_at {
        return Ok(());
    }
    if p.status == "deleted" {
        return engine.remove(p).await.map_err(failure);
    }
    if deploy {
        if std::env::var("NEXT_PUBLIC_CBGPU_ACCOUNT_V2").as_deref() == Ok("true")
            && !api
                .resource_allowed(&current.user_id, "school_compute")
                .await
                .map_err(failure)?
        {
            return Err(failure(Failure("School compute permission unavailable")));
        }
        let diagnostics_db = std::env::var("CBGPU_CPU_DIAGNOSTICS_DB").as_deref() == Ok("true");
        if diagnostics_db {
            api.cpu_patch(&p.id, serde_json::json!({"deployment_attempted_at":chrono::Utc::now(),"deployment_error":null})).await.map_err(failure)?;
        }
        if let Err(error) = engine.deploy(&current).await {
            if diagnostics_db {
                let _ = api.cpu_patch(&p.id, serde_json::json!({"deployment_logs":error.logs,"deployment_error":error.message,"deployment_diagnostics":{"health":error.health,"rollback_restored":error.rollback_restored}})).await;
            }
            return Err(error);
        }
        if diagnostics_db {
            let _ = api
                .cpu_patch(
                    &p.id,
                    serde_json::json!({"deployment_error":null,"deployment_diagnostics":null}),
                )
                .await;
        }
    }
    // Tunnel failures never falsely roll back an otherwise healthy application; next poll retries.
    if let Ok(url) = engine.sync_tunnel(&current).await
        && url != p.public_url
    {
        api.cpu_patch_current(&current, serde_json::json!({"public_url":url}))
            .await
            .map_err(failure)?;
    }
    if deploy {
        api.cpu_patch_current(
            &current,
            serde_json::json!({"deployed_at":chrono::Utc::now()}),
        )
        .await
        .map_err(failure)?;
    }
    Ok(())
}
