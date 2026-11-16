# GPU notebook SSH integration

`Dockerfile` retains the vendor TensorFlow/Jupyter image, installs OpenSSH and
uses the native Rust `start.rs` entrypoint. Startup preserves `authorized_keys`,
enforces private permissions, refuses symlinked key files and starts the vendor
notebook process. It never manages host containers or bookings. Image builds do
not replace existing GPU containers.

Persistent GPU host keys stay in `docker.ssh_host_keys_dir`, mounted read-only
into replacements. Keep original containers, preserved images and user files
until a separately reviewed recovery/retention decision. Never reset an active
GPU merely to apply a port, image or startup change.

The native `cbgpu-ssh-gateway` replaces the retired Go implementation. It accepts
registered Ed25519 keys and fixed reservation-scoped GPU forwards; host shells
and arbitrary destinations are denied. Fresh bookings/keys govern authentication,
channel creation and ongoing connections. Expiry, cancellation, conflicting owners
and backend failures deny access. Both loopback/Funnel and school-LAN units under
`deploy/systemd` reuse the existing gateway host key.

Follow [native staging and handover](../docs/INSTALLATION.md). Installed user
services remain independent until the operator safely transfers ownership. The
installer does not stop them or change the Tailscale Funnel route. Do not bind a
new gateway to an already-owned listener. Another host requires explicit review
of the fixed destination address and network policy.
