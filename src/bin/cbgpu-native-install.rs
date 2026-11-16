use cbgpu_gpu_daemon::{
    authorization::{Failure, Result},
    installer::{self, Layout, NativeHost, Settings},
};
use std::path::PathBuf;
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1)
    }
}
async fn run() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() || args.iter().any(|a| a == "--help") {
        println!(
            "cbgpu-native-install stage|approve --version=VERSION --config=PRIVATE_YAML --service-key=PRIVATE_KEY --ssh-key=EXISTING_HOST_KEY --binaries=RELEASE_BIN_DIR [--legacy-user=cbgpu] [--apply] [--cpu-jobs-drained]\nDefault is a side-effect-free plan. Stage installs disabled/unapproved native units and private backups only. Approval requires inactive legacy/native duties; it never stops, starts, enables or restarts any service/container. No apt, Docker runtime or driver changes."
        );
        return Ok(());
    }
    let value = |key: &str| {
        args.iter()
            .find_map(|v| v.strip_prefix(&format!("--{key}=")))
            .map(str::to_owned)
    };
    let settings = Settings {
        version: value("version").unwrap_or_else(|| env!("CARGO_PKG_VERSION").into()),
        config: value("config").map(PathBuf::from).unwrap_or_default(),
        service_key: value("service-key").map(PathBuf::from).unwrap_or_default(),
        ssh_key: value("ssh-key").map(PathBuf::from).unwrap_or_default(),
        binaries: value("binaries")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("target/release")),
        legacy_user: value("legacy-user").unwrap_or_else(|| "cbgpu".into()),
    };
    if !["stage", "approve"].contains(&args[0].as_str()) {
        return Err(Failure("Use stage or approve"));
    }
    if !args.iter().any(|v| v == "--apply") {
        println!("{}", installer::plan(&settings));
        return Ok(());
    }
    if ![
        &settings.config,
        &settings.service_key,
        &settings.ssh_key,
        &settings.binaries,
    ]
    .iter()
    .all(|p| p.is_absolute())
    {
        return Err(Failure(
            "Explicit apply requires absolute config, credentials and binary paths",
        ));
    }
    let layout = Layout {
        root: PathBuf::from("/"),
    };
    let manifest = if args[0] == "stage" {
        installer::stage(&settings, &layout, &NativeHost).await?
    } else {
        installer::approve(
            &settings,
            &layout,
            &NativeHost,
            args.iter().any(|v| v == "--cpu-jobs-drained"),
        )
        .await?
    };
    println!(
        "{}",
        serde_json::json!({"release":manifest.release,"approval":manifest.approval,"approved":manifest.approved,"services_started":false,"containers_changed":false})
    );
    Ok(())
}
