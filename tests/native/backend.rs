use cbgpu_gpu_daemon::authorization::{Authorizer, RestAuthorizer};
use chrono::Utc;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
async fn server(
    status: u16,
    body: &str,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();
    let body = body.to_owned();
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            c.fetch_add(1, Ordering::SeqCst);
            let mut bytes = vec![0; 8192];
            let _ = stream.read(&mut bytes).await;
            let response = format!(
                "HTTP/1.1 {status} response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    (url, counter, task)
}
#[tokio::test]
async fn transient_backend_error_retries_three_times_without_leaking_credentials() {
    let (url, count, task) = server(500, "PRIVATE_PROVIDER_DETAIL").await;
    let api = RestAuthorizer::new(&url, "PRIVATE_SERVICE_TOKEN".into()).unwrap();
    let error = api.allowed("unused", Utc::now()).await.unwrap_err();
    assert_eq!(count.load(Ordering::SeqCst), 3);
    assert!(!error.to_string().contains("PRIVATE"));
    task.abort();
}
#[tokio::test]
async fn unauthorized_backend_is_not_retried_and_never_grants() {
    let (url, count, task) = server(401, "PRIVATE_PROVIDER_DETAIL").await;
    let api = RestAuthorizer::new(&url, "PRIVATE_SERVICE_TOKEN".into()).unwrap();
    assert!(api.allowed("unused", Utc::now()).await.is_err());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    task.abort();
}
#[tokio::test]
async fn empty_approved_booking_response_denies_without_key_fetch() {
    let (url, count, task) = server(200, "[]").await;
    let api = RestAuthorizer::new(&url, "PRIVATE_SERVICE_TOKEN".into()).unwrap();
    assert!(api.allowed("unused", Utc::now()).await.unwrap().is_empty());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    task.abort();
}
#[tokio::test]
async fn oversized_backend_json_rejected_before_parsing() {
    let (url, _, task) = server(200, &"a".repeat((1 << 20) + 1)).await;
    let api = RestAuthorizer::new(&url, "PRIVATE_SERVICE_TOKEN".into()).unwrap();
    assert!(
        api.allowed("unused", Utc::now())
            .await
            .unwrap_err()
            .to_string()
            .contains("size")
    );
    task.abort();
}
#[tokio::test]
async fn stale_deployment_completion_is_not_allowed_to_acknowledge_newer_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = vec![0; 8192];
        let n = stream.read(&mut bytes).await.unwrap();
        let request = String::from_utf8_lossy(&bytes[..n]);
        assert!(request.starts_with("PATCH "));
        let target = request
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        let parsed = reqwest::Url::parse(&format!("http://fixture{target}")).unwrap();
        let query: std::collections::BTreeMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(query.get("status").unwrap(), "eq.approved");
        assert_eq!(query.get("select").unwrap(), "id");
        assert_eq!(
            query.get("deploy_requested_at").unwrap(),
            "eq.2026-11-16T00:00:00+00:00"
        );
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]").await.unwrap();
    });
    let p=serde_json::from_value(serde_json::json!({"id":"11111111-2222-4333-8444-555555555555","status":"approved","host_port":31555,"container_port":8080,"cpu_limit":"3.0","memory_limit":"4g","deploy_requested_at":"2026-11-16T00:00:00Z"})).unwrap();
    let api = RestAuthorizer::new(&url, "fixture-service-token".into()).unwrap();
    assert!(
        !api.cpu_patch_current(
            &p,
            serde_json::json!({"deployed_at":"2026-11-16T01:00:00Z"})
        )
        .await
        .unwrap()
    );
    task.await.unwrap();
}
