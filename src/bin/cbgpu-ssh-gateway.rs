use cbgpu_gpu_daemon::{
    authorization::{Failure, RestAuthorizer},
    gateway,
};
use std::{env, fs, path::Path, sync::Arc};
fn private_file(path: &Path) -> Result<(), Failure> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| Failure("Required private file unavailable"))?;
    if !metadata.is_file() {
        return Err(Failure("Invalid private file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Failure("Private credential files must have mode 600"));
        }
    }
    Ok(())
}
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1)
    }
}
async fn run() -> Result<(), Failure> {
    if env::args().any(|arg| arg == "--help") {
        println!(
            "CBGPU native GPU SSH gateway. Configure CBGPU_SERVICE_ROLE_KEY_FILE, CBGPU_GATEWAY_HOST_KEY, CBGPU_SUPABASE_URL, and optional CBGPU_GATEWAY_LISTEN_ADDRESS. No container changes are performed."
        );
        return Ok(());
    }
    let secret = env::var("CBGPU_SERVICE_ROLE_KEY_FILE")
        .map_err(|_| Failure("Gateway configuration required"))?;
    let host = env::var("CBGPU_GATEWAY_HOST_KEY")
        .map_err(|_| Failure("Gateway configuration required"))?;
    let url =
        env::var("CBGPU_SUPABASE_URL").map_err(|_| Failure("Gateway configuration required"))?;
    private_file(Path::new(&secret))?;
    private_file(Path::new(&host))?;
    let secret =
        fs::read_to_string(secret).map_err(|_| Failure("Service credential unavailable"))?;
    let auth = RestAuthorizer::new(&url, secret)?;
    let key =
        russh::keys::load_secret_key(&host, None).map_err(|_| Failure("Invalid SSH host key"))?;
    if key.algorithm() != russh::keys::Algorithm::Ed25519 {
        return Err(Failure("An Ed25519 SSH host key is required"));
    }
    let address =
        env::var("CBGPU_GATEWAY_LISTEN_ADDRESS").unwrap_or_else(|_| "127.0.0.1:22220".into());
    let socket: std::net::SocketAddr = address
        .parse()
        .map_err(|_| Failure("Invalid gateway listen address"))?;
    if socket.ip().is_unspecified() {
        return Err(Failure(
            "Gateway must bind an explicit private or loopback address",
        ));
    }
    let listener = tokio::net::TcpListener::bind(socket)
        .await
        .map_err(|_| Failure("Gateway listener unavailable"))?;
    eprintln!("CBGPU native SSH gateway listening on {socket}");
    gateway::serve(
        listener,
        Arc::new(gateway::config(key)),
        Arc::new(auth),
        gateway::Settings::default(),
    )
    .await
}
