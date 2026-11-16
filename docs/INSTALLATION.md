# Native controller installation and ownership handover

The native workers manage GPU reservations, non-Compose CPU deployments and
reservation-scoped SSH. `cbgpu-native-install` stages their files and verifies an
ownership handover. These are Linux server tools, not workstation applications.
Docker and the NVIDIA driver/toolkit must already be configured by the operator.
The installer does not install packages, disable OS updates, change Docker/NVIDIA
configuration, restart Docker or restart notebooks.

## Build and review

```bash
cargo build --release --locked
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
target/release/cbgpu-native-install --help
```

Prepare private YAML outside Git using `config.yaml` as a sample. Set the actual
backend URL, GPU/device/port mappings and school bind address. Keep
`initialize_idle_containers: false`. Keep `docker.ssh_host_keys_dir` pointed at the
existing persistent GPU host keys. Existing Jupyter password configuration is
supported; obsolete NativeAuthenticator command adapters are rejected rather
than silently treated as compatible.

Keep the service-role credential and existing Ed25519 SSH gateway host key in
separate mode-600 files. Do not rotate the gateway key during migration. Never
put credentials in YAML samples, commands, Git or logs. The fixed GPU destination
allowlist in `src/gateway.rs` needs explicit review for another host.

## Staging does not activate services

```bash
/absolute/target/release/cbgpu-native-install stage \
  --version=0.1.0 \
  --config=/private/controller.yaml \
  --service-key=/private/service-role-key \
  --ssh-key=/private/existing-gateway-host-key \
  --binaries=/absolute/target/release
```

Without `--apply`, the command prints a plan without reading credentials,
contacting services or creating files. After review, run the same command as root
with `--apply` to stage immutable version/digest binaries, private configuration
and credential copies under `/etc/cbgpu`, private backups under
`/var/lib/cbgpu-native-install/backups`, and unapproved GPU/CPU/loopback-SSH/LAN-SSH
unit definitions. Key bytes are preserved.

Only unit definitions are reloaded. No service is enabled, started, stopped or
restarted, no container/volume is replaced, and no approval marker is created.
Active native workers, existing drop-ins, symlink destinations, unsafe input
permissions and silent credential rotation are refused. Legacy services may
continue running while this unapproved release is staged.

## Ownership approval is separate

The installed legacy `/home/cbgpu/CBGPU/Daemon/daemon.py` is separate from this
source repository. Its GPU/CPU duties and user SSH gateways must be inactive and
disabled/masked before approval. The installer never stops them. The legacy
process has no verified cooperative pause/state-export protocol; active or
unknown ownership is rejected. SIGSTOP is not proof that Docker jobs have drained.

Drain and review in-flight CPU jobs and existing SSH forwards before stopping
legacy services; stopping a gateway can close users’ SSH connections. This tool
does not transfer live SSH sessions between implementations. `--cpu-jobs-drained` records the
operator's confirmation; it cannot prove that an orphaned Docker build finished.
Preserve the immutable legacy runtime, configuration, containers, volumes and
host keys for recovery. Staging/approval never deletes them.

Run `approve` with the same absolute paths/version and `--legacy-user=cbgpu` (or
the actual user-service owner). It defaults to a plan. Explicit root
`approve --apply --cpu-jobs-drained` verifies inactive duties, disabled/masked
legacy services, exact reviewed file hashes, unchanged notebook IDs/running
states/restart counts, existing running-notebook authentication configuration and
a fresh backend snapshot. It checks ownership again before writing the private
release-specific receipt. That receipt does not start services.

Review the manifest and native `--plan-once` output before the separate operator
action to enable/start native units. Keep the old controller disabled so it
cannot regain ownership after boot. Both SSH listeners preserve the existing key
and explicit loopback/LAN addresses. The Funnel route is unchanged.

Account V2 and database-backed CPU diagnostics stay off by default. This tool
never applies their migrations or opens school resources to external users.
Complete runtime resource-lease revocation monitoring before enabling V2 for
running CPU workloads, as described in README.md.

## Recovery and boundaries

Failures do not trigger notebook resets or service restarts; definitions remain
unapproved. Recover inactive files from the private backup only after comparing
hashes and concurrent operator changes. Never delete notebook volumes during
recovery. Fixed-port CPU redeployments still have a stop/copy/start window;
installation does not make those deployments zero downtime.

Authored container startup is Rust (`gpu_ssh/start.rs`). Its image build uses a
static-musl Rust builder and the existing vendor TensorFlow/Jupyter runtime.
Vendor environment initialization remains delegated to Bash. No maintained
Python, Go or shell implementation is installed from this repository.

The controller’s initial preservation path assumes a nonempty Jupyter auth file
already contains the backend’s current password. Confirm legacy password
synchronization before handover; the installer checks presence, not hash parity.
A pending password change needs separate review so kernel preservation does not
leave stale login credentials.
