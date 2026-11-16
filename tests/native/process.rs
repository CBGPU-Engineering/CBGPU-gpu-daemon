use cbgpu_gpu_daemon::docker::{NativeRunner, Runner};
#[tokio::test]
async fn verbose_commands_are_drained_without_deadlocking_and_keep_bounded_tail() {
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&vec![b'x'; 2000000]).unwrap();
    file.write_all(b"END").unwrap();
    file.flush().unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        NativeRunner.run("cat", &[file.path().to_string_lossy().into_owned()], None),
    )
    .await
    .expect("verbose output must not deadlock")
    .unwrap();
    assert!(result.success);
    assert!(result.stdout.starts_with("[Earlier output truncated]"));
    assert!(result.stdout.ends_with("xEND"));
    assert!(result.stdout.len() < 66000);
}
#[tokio::test]
async fn command_input_and_output_are_drained_concurrently() {
    let input = vec![b'x'; 2000000];
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        NativeRunner.run("cat", &[], Some(input)),
    )
    .await
    .expect("piped input/output must not deadlock")
    .unwrap();
    assert!(result.success);
    assert!(result.stdout.len() < 66000);
}
