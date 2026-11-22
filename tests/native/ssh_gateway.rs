use cbgpu_gpu_daemon::{
    authorization::{self, Authorizer, RegisteredKey, Reservation, Result},
    gateway,
};
use chrono::{Duration, Utc};
use russh::{
    client,
    keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate},
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
fn key(seed: u8) -> PrivateKey {
    russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[seed; 32]).into()
}
struct Auth {
    bookings: Mutex<Vec<Reservation>>,
    keys: Vec<RegisteredKey>,
    fail: AtomicBool,
}
#[async_trait::async_trait]
impl Authorizer for Auth {
    async fn allowed(
        &self,
        key: &str,
        at: chrono::DateTime<Utc>,
    ) -> Result<BTreeMap<u8, chrono::DateTime<Utc>>> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(authorization::Failure("Synthetic database outage"));
        }
        authorization::allowed_from_rows(&self.bookings.lock().unwrap(), &self.keys, key, at)
    }
}
fn booking(user: &str, gpu: u8) -> Reservation {
    Reservation {
        id: format!("reservation-{gpu}"),
        user_id: user.into(),
        gpu_id: gpu,
        start_time: Utc::now() - Duration::minutes(1),
        end_time: Utc::now() + Duration::minutes(2),
        status: "approved".into(),
    }
}
#[test]
fn plain_ed25519_only_no_authorized_keys_options() {
    let public = key(1).public_key().to_openssh().unwrap();
    assert_eq!(
        authorization::validate_public_key(&format!("{public} comment")).unwrap(),
        public
    );
    for bad in [
        format!("command=\"id\" {public}"),
        "ssh-ed25519 AAAA".into(),
        public.replace("ssh-ed25519", "ssh-rsa"),
    ] {
        assert!(authorization::validate_public_key(&bad).is_err())
    }
}
#[test]
fn live_approved_booking_matching_key_and_no_conflicting_owner_required() {
    let public = key(1).public_key().to_openssh().unwrap();
    let keys = vec![RegisteredKey {
        user_id: "a".into(),
        public_key: public.clone(),
    }];
    let bookings = vec![booking("a", 1), booking("b", 2)];
    let allowed = authorization::allowed_from_rows(&bookings, &keys, &public, Utc::now()).unwrap();
    assert_eq!(allowed.len(), 1);
    assert!(allowed.contains_key(&1));
    assert!(
        authorization::allowed_from_rows(
            &bookings,
            &keys,
            &key(2).public_key().to_openssh().unwrap(),
            Utc::now()
        )
        .unwrap()
        .is_empty()
    );
    let mut conflict = bookings.clone();
    conflict.push(booking("b", 1));
    assert!(
        authorization::allowed_from_rows(&conflict, &keys, &public, Utc::now())
            .unwrap()
            .is_empty()
    );
    let mut denied = bookings.clone();
    denied[0].status = "pending".into();
    assert!(
        authorization::allowed_from_rows(&denied, &keys, &public, Utc::now())
            .unwrap()
            .is_empty()
    );
    assert!(
        authorization::allowed_from_rows(
            &bookings,
            &keys,
            &public,
            Utc::now() + Duration::minutes(3)
        )
        .unwrap()
        .is_empty()
    );
}
#[test]
fn only_fixed_gpu_destinations_allowed() {
    for gpu in 1..=4 {
        assert_eq!(
            gateway::destination_gpu(gateway::GPU_ADDRESS, 2200 + gpu),
            Some(gpu as u8)
        )
    }
    for (host, port) in [
        ("127.0.0.1", 2204),
        (gateway::GPU_ADDRESS, 22),
        (gateway::GPU_ADDRESS, 8888),
        (gateway::GPU_ADDRESS, 2205),
        ("example.org", 2204),
    ] {
        assert_eq!(gateway::destination_gpu(host, port), None)
    }
}
struct Client {
    host: PublicKey,
}
impl client::Handler for Client {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        Ok(key.certificate().is_none() && key.public_key() == self.host)
    }
}
#[tokio::test]
async fn actual_native_ssh_forwarding_half_close_and_revocation() {
    let user_key = Arc::new(key(1));
    let host_key = key(3);
    let host_pub = host_key.public_key().clone();
    let public = user_key.public_key().to_openssh().unwrap();
    let auth = Arc::new(Auth {
        bookings: Mutex::new(vec![booking("a", 4)]),
        keys: vec![RegisteredKey {
            user_id: "a".into(),
            public_key: public,
        }],
        fail: AtomicBool::new(false),
    });
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = backend.local_addr().unwrap();
    let echo = tokio::spawn(async move {
        loop {
            let (mut stream, _) = backend.accept().await.unwrap();
            tokio::spawn(async move {
                let (mut reader, mut writer) = stream.split();
                tokio::io::copy(&mut reader, &mut writer).await.unwrap();
                let _ = writer.write_all(b"after-eof").await;
            });
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let settings = gateway::Settings {
        targets: [target; 4],
        recheck: std::time::Duration::from_millis(50),
        ..Default::default()
    };
    let gateway_auth = auth.clone();
    let serve = tokio::spawn(gateway::serve(
        listener,
        Arc::new(gateway::config(host_key)),
        gateway_auth,
        settings,
    ));
    let mut client = client::connect(
        Arc::new(client::Config::default()),
        address,
        Client { host: host_pub },
    )
    .await
    .unwrap();
    let accepted = client
        .authenticate_publickey("gateway", PrivateKeyWithHashAlg::new(user_key, None))
        .await
        .unwrap();
    assert!(accepted.success());
    assert!(client.channel_open_session().await.is_err());
    assert!(
        client
            .channel_open_direct_tcpip(gateway::GPU_ADDRESS, 2203, "127.0.0.1", 1234)
            .await
            .is_err()
    );
    assert!(
        client
            .channel_open_direct_tcpip("127.0.0.1", 22, "127.0.0.1", 1234)
            .await
            .is_err()
    );
    let channel = client
        .channel_open_direct_tcpip(gateway::GPU_ADDRESS, 2204, "127.0.0.1", 1234)
        .await
        .unwrap();
    let mut stream = channel.into_stream();
    stream.write_all(b"gpu-ready").await.unwrap();
    let mut bytes = [0; 9];
    stream.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"gpu-ready");
    let half = client
        .channel_open_direct_tcpip(gateway::GPU_ADDRESS, 2204, "127.0.0.1", 1234)
        .await
        .unwrap();
    let mut half = half.into_stream();
    half.write_all(b"half-close").await.unwrap();
    let mut bytes = [0; 10];
    half.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"half-close");
    half.shutdown().await.unwrap();
    let mut tail = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        half.read_to_end(&mut tail),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(tail, b"after-eof");
    // A fresh, still-active reservation extension must keep this existing channel alive.
    let old_end = Utc::now() + Duration::milliseconds(250);
    auth.bookings.lock().unwrap()[0].end_time = old_end;
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    auth.bookings.lock().unwrap()[0].end_time = Utc::now() + Duration::seconds(2);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    stream.write_all(b"extended!!").await.unwrap();
    stream.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"extended!!");
    auth.bookings.lock().unwrap().clear();
    let mut byte = [0];
    let closed = tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut byte))
        .await
        .unwrap();
    assert!(closed.is_err() || closed.unwrap() == 0);
    let _ = client
        .disconnect(russh::Disconnect::ByApplication, "test complete", "en")
        .await;
    serve.abort();
    echo.abort();
}
