use crate::authorization::{Authorizer, Failure, Result, validate_public_key};
use chrono::{DateTime, Utc};
use russh::{
    Channel, ChannelOpenFailure,
    keys::Algorithm,
    server::{self, Auth, ChannelOpenHandle, Msg, Session},
};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{Instant, sleep_until},
};
pub const GPU_ADDRESS: &str = "100.100.136.37";
#[derive(Clone)]
pub struct Settings {
    pub targets: [SocketAddr; 4],
    pub recheck: Duration,
    pub max_connections: usize,
    pub max_forwards_per_connection: usize,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            targets: std::array::from_fn(|i| {
                format!("{GPU_ADDRESS}:{}", 2201 + i)
                    .parse()
                    .expect("fixed GPU address")
            }),
            recheck: Duration::from_secs(15),
            max_connections: 64,
            max_forwards_per_connection: 64,
        }
    }
}
pub fn destination_gpu(host: &str, port: u32) -> Option<u8> {
    if host == GPU_ADDRESS && (2201..=2204).contains(&port) {
        Some((port - 2200) as u8)
    } else {
        None
    }
}
fn deadline(end: DateTime<Utc>) -> Instant {
    Instant::now() + (end - Utc::now()).to_std().unwrap_or_default()
}
struct Connection {
    auth: Arc<dyn Authorizer>,
    settings: Settings,
    key: Option<String>,
    authenticated: Arc<AtomicBool>,
    channels: Arc<Semaphore>,
}
impl server::Handler for Connection {
    type Error = russh::Error;
    async fn auth_publickey_offered(
        &mut self,
        user: &str,
        key: server::PublicKeyOffer<'_>,
    ) -> std::result::Result<Auth, Self::Error> {
        Ok(
            if user == "gateway"
                && *key.algorithm == Algorithm::Ed25519
                && key.public_key.algorithm() == Algorithm::Ed25519
            {
                Auth::Accept
            } else {
                Auth::reject()
            },
        )
    }
    async fn auth_publickey(
        &mut self,
        user: &str,
        key: server::VerifiedPublicKey<'_>,
    ) -> std::result::Result<Auth, Self::Error> {
        if user != "gateway"
            || *key.algorithm != Algorithm::Ed25519
            || key.public_key.algorithm() != Algorithm::Ed25519
        {
            return Ok(Auth::reject());
        }
        let key = match key
            .public_key
            .to_openssh()
            .ok()
            .and_then(|value| validate_public_key(&value).ok())
        {
            Some(key) => key,
            None => return Ok(Auth::reject()),
        };
        if self
            .auth
            .allowed(&key, Utc::now())
            .await
            .is_ok_and(|allowed| !allowed.is_empty())
        {
            self.key = Some(key);
            self.authenticated.store(true, Ordering::Release);
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        _origin: &str,
        _origin_port: u32,
        reply: ChannelOpenHandle,
        session: &mut Session,
    ) -> std::result::Result<(), Self::Error> {
        let Some(gpu) = destination_gpu(host, port) else {
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        };
        let Some(key) = self.key.clone() else {
            return Ok(());
        };
        let Ok(permit) = self.channels.clone().try_acquire_owned() else {
            return Ok(());
        };
        let auth = self.auth.clone();
        let settings = self.settings.clone();
        let handle = session.handle();
        tokio::spawn(async move {
            let _permit = permit;
            let end = match auth.allowed(&key, Utc::now()).await {
                Ok(allowed) => match allowed.get(&gpu) {
                    Some(end) => *end,
                    None => return,
                },
                Err(_) => return,
            };
            if end <= Utc::now() {
                return;
            }
            let backend = match tokio::time::timeout(
                Duration::from_secs(5),
                TcpStream::connect(settings.targets[(gpu - 1) as usize]),
            )
            .await
            {
                Ok(Ok(stream)) => stream,
                _ => {
                    reply.reject(ChannelOpenFailure::ConnectFailed).await;
                    return;
                }
            };
            if Utc::now() >= end {
                return;
            }
            reply.accept().await;
            relay(
                channel,
                backend,
                auth,
                key,
                gpu,
                end,
                settings.recheck,
                handle,
            )
            .await;
        });
        Ok(())
    }
}
#[allow(clippy::too_many_arguments)] // Each connection carries independent authorization and expiry state.
async fn relay(
    channel: Channel<Msg>,
    mut backend: TcpStream,
    auth: Arc<dyn Authorizer>,
    key: String,
    gpu: u8,
    mut end: DateTime<Utc>,
    recheck: Duration,
    handle: server::Handle,
) {
    let channel_id = channel.id();
    let mut stream = channel.into_stream();
    let transfer = tokio::io::copy_bidirectional(&mut stream, &mut backend);
    tokio::pin!(transfer);
    let mut tick = tokio::time::interval(recheck);
    tick.tick().await;
    loop {
        tokio::select! {_=&mut transfer=>break,_=sleep_until(deadline(end))=>break,_=tick.tick()=>{
         let authorization=auth.allowed(&key,Utc::now());tokio::pin!(authorization);
         let fresh=tokio::select!{_=&mut transfer=>break,_=sleep_until(deadline(end))=>break,result=&mut authorization=>result};
         match fresh{Ok(allowed)=>match allowed.get(&gpu){Some(next)if *next>Utc::now()=>{end=end.min(*next)},_=>break},Err(_)=>break}
        }}
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.close(channel_id)).await;
    // Dropping the SSH stream and TCP socket closes both directions even on revocation/lookup failure.
}
/// Serve only on an already-bound socket. Startup performs no Docker/reservation mutations.
pub async fn serve(
    listener: TcpListener,
    config: Arc<server::Config>,
    auth: Arc<dyn Authorizer>,
    settings: Settings,
) -> Result<()> {
    let capacity = Arc::new(Semaphore::new(settings.max_connections));
    loop {
        let (socket, _) = listener
            .accept()
            .await
            .map_err(|_| Failure("Gateway listener unavailable"))?;
        let Ok(permit) = capacity.clone().try_acquire_owned() else {
            drop(socket);
            continue;
        };
        let config = config.clone();
        let auth = auth.clone();
        let settings = settings.clone();
        tokio::spawn(async move {
            serve_connection(socket, config, auth, settings, permit).await;
        });
    }
}
async fn serve_connection(
    socket: TcpStream,
    config: Arc<server::Config>,
    auth: Arc<dyn Authorizer>,
    settings: Settings,
    _permit: OwnedSemaphorePermit,
) {
    let authenticated = Arc::new(AtomicBool::new(false));
    let connection = Connection {
        auth,
        channels: Arc::new(Semaphore::new(settings.max_forwards_per_connection)),
        settings,
        key: None,
        authenticated: authenticated.clone(),
    };
    let session = match tokio::time::timeout(
        Duration::from_secs(30),
        server::run_stream(config, socket, connection),
    )
    .await
    {
        Ok(Ok(session)) => session,
        _ => return,
    };
    let handle = session.handle();
    let guard = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        if !authenticated.load(Ordering::Acquire) {
            let _ = handle
                .disconnect(
                    russh::Disconnect::ByApplication,
                    "Authentication timed out".into(),
                    "".into(),
                )
                .await;
        }
    });
    let _ = session.await;
    guard.abort();
}
pub fn config(host_key: russh::keys::PrivateKey) -> server::Config {
    server::Config {
        keys: vec![host_key],
        max_auth_attempts: 3,
        auth_rejection_time: Duration::from_secs(1),
        auth_rejection_time_initial: Some(Duration::ZERO),
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    }
}
