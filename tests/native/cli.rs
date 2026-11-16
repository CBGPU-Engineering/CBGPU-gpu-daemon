use std::process::Command;
#[test]
fn configuration_check_does_not_need_network_docker_or_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.yaml");
    std::fs::write(&config,"supabase_url: http://127.0.0.1:1\nsupabase_anon_key: fixture\ngpus:\n  1:\n    username: gpu1\n    container: fixture_gpu1\n    device: '0'\n    host_port: 8001\n    ssh_port: 2201\ndocker:\n  image: fixture/gpu:latest\n  container_port: 8888\n  school_bind_address: 10.138.23.12\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cbgpu-gpu-controller"))
        .arg("--check")
        .arg(&config)
        .env("PATH", "/nonexistent")
        .env("CBGPU_SUPABASE_SERVICE_ROLE_KEY_FILE", "/nonexistent")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("no backend or Docker"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}
#[test]
fn controller_refuses_activation_without_explicit_apply() {
    let output = Command::new(env!("CARGO_BIN_EXE_cbgpu-gpu-controller"))
        .args(["--run", "/nonexistent"])
        .env("PATH", "/nonexistent")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--run --apply"));
}
#[test]
fn native_help_is_side_effect_free() {
    for binary in [
        env!("CARGO_BIN_EXE_cbgpu-gpu-controller"),
        env!("CARGO_BIN_EXE_cbgpu-cpu-controller"),
        env!("CARGO_BIN_EXE_cbgpu-ssh-gateway"),
    ] {
        let output = Command::new(binary)
            .arg("--help")
            .env("PATH", "/nonexistent")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(!output.stdout.is_empty());
    }
}
