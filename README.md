# CBGPU GPU Daemon

[![CI](https://github.com/CBGPU-Engineering/CBGPU-gpu-daemon/actions/workflows/ci.yml/badge.svg)](https://github.com/CBGPU-Engineering/CBGPU-gpu-daemon/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-Native%20controller-000000?logo=rust)](https://www.rust-lang.org)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-D22128)](LICENSE)

The host-side GPU reservation controller, managed container lifecycle and SSH
gateway for CBGPU. This is a privileged infrastructure component, not the web UI.

## Native Rust candidate

The native implementation provides three independent binaries: the GPU booking
controller, an SSH forwarding gateway, and the non-Compose CPU deployment worker.
CPU builds run separately so installation cannot postpone a known GPU expiry.
A separate installed legacy service remains available during the reviewed handover;
building or testing this repository does not restart it. Maintained host logic is Rust.

```bash
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
# Optional, isolated real CPU deployment / health-failure rollback fixture:
cargo test --test cpu_docker -- --ignored
# Configuration validation only; no backend or Docker operations:
target/release/cbgpu-gpu-controller --check config.yaml
```

The GPU controller defaults to preserving running and idle notebooks on initial
observation. It retains session deadlines in private atomic state, enforces known
expiry during backend outages, reuses SSH host keys, and stages replacements before
stopping the existing container. Failed activation restores the previous container.
Previous container filesystems and persistent data volumes remain available for
reviewed retention and recovery.

The SSH gateway accepts signed Ed25519 authentication and fixed GPU SSH forwards
only. Authentication, channel opening and ongoing connections require fresh
approved bookings and registered keys. Conflicting owners deny access; expiry and
revocation close active forwards. Shells, host SSH forwarding and arbitrary targets
are not offered.

The CPU worker supports Docker images and the existing Node, Python, Java, Go and
.NET Git runtimes. It preserves Root Directory, custom start commands, environments,
16 GB RAM allocations and persistent `/app` storage. Environments are staged in
private temporary files rather than process arguments. Two deployment jobs can run
concurrently; failed revisions retry after five minutes. Failed startup records
redacted health and container diagnostics and restores the old deployment.
Public projects use scoped Cloudflare quick tunnels; private and school projects
bind explicit interfaces. Compose projects remain with the separate Compose worker.

Activation requires a reviewed handover and explicit `--run --apply`; no service
unit or setup script activates the Rust workers automatically. Run GPU and CPU
workers with their separate private state locations and ensure the old controller
no longer owns those duties before enabling them. Drain in-flight legacy CPU jobs,
verify a healthy backend snapshot and run `--plan-once` before transferring ownership.
Do not overlap old and new deployment controllers; native worker locks protect only
other native workers. Private files must have mode
`600`, their directories `700`, and secrets must remain outside Git.

The [CPU diagnostics migration](https://github.com/CBGPU-Engineering/CBGPU-backend/blob/main/supabase/migrations/20261116120100_cpu_deployment_diagnostics.sql)
belongs to the backend repository and is a **candidate**, not an installation command. It adds bounded diagnostics and a database trigger that
prevents browser roles from forging logs even with broad existing table grants.
Enable `CBGPU_CPU_DIAGNOSTICS_DB=true` only after that migration has passed the
backend's review/deployment process. Until then, the worker writes private atomic
JSON diagnostics. No application projects, account IDs, bookings or volumes are
recreated by this migration.

### Service handover

[Example systemd units](deploy/systemd/) invoke the native binaries from a
versioned `/opt/cbgpu-native/0.1.0/bin` installation, not a mutable Git checkout.
They remain examples until the native installer explicitly stages them. Staging
uses immutable version/digest directories and a release-specific private approval
receipt; it does not enable, stop, start or restart services. Keep the service key private, preserve
the existing gateway host key, and configure `docker.ssh_host_keys_dir` under
`/var/lib/cbgpu-gpu-controller/host-keys` when using these examples.

The current host still runs the independent legacy installation at
`/home/cbgpu/CBGPU/Daemon/daemon.py`; publishing Rust source does not change that
process. Legacy setup/Python/Go sources have been retired from this repository. The
independent installed tree is preserved until the operator completes a safe
handover. No source publication changes running services.

Fixed-port application redeployments stop the old container during the storage
copy/port handoff; rollback is provided, but this is not a zero-downtime swap.
Git `/app` storage is copied to a new persistent volume. Docker-image anonymous
volumes remain with the previous container for recovery and are not automatically
reused by the replacement. Managed runtime image tags retain the existing
upstream update policy. Before enabling account V2 for running CPU workloads,
add the corresponding runtime lease/revocation monitoring: the current native
worker checks fresh resource permission before launch.

## Tests

```bash
git clone https://github.com/CBGPU-Engineering/CBGPU-gpu-daemon.git
cd CBGPU-gpu-daemon
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
rustc --edition=2024 --test gpu_ssh/start.rs -o /tmp/cbgpu-entrypoint-test
/tmp/cbgpu-entrypoint-test
```

Installer fixtures fake systemctl, Docker observations and local units; they never
stop or restart a service. The optional real CPU deployment fixture uses only its
own disposable containers. Container startup is native Rust; the vendor Jupyter
and TensorFlow image and its environment initialization remain third-party code.

## Installation

`cbgpu-native-install stage` defaults to a side-effect-free plan. Explicit root
`stage --apply` backs up private units/environment files, preserves credentials
and existing notebook IDs, and installs unapproved service definitions only. A
separate `approve --apply --cpu-jobs-drained` requires inactive/disabled legacy
controllers and SSH gateways, fresh backend checks and unchanged reviewed files.
It writes a private receipt; service activation remains a separate operator action.
No package installation, Docker/driver reconfiguration or OS tuning is performed.


Use [CBGPU-platform](https://github.com/CBGPU-Engineering/CBGPU-platform) for the existing `Daemon/` integration
path. Review [installation details](docs/INSTALLATION.md) and configure your own
backend URL, keys, GPU/device mappings, storage, bind addresses and service units.
Do not run setup scripts or the controller on an arbitrary workstation just to
try the project; use the web demo instead. Runtime service-role keys and host SSH
keys stay outside Git. Existing public client defaults in the historical sample
are not credentials for another installation.

Original code is [Apache-2.0](LICENSE); see [history provenance](docs/MIGRATION.md).
