# Changelog

## Unreleased

- Add a native staging/ownership installer with private backups, immutable release
  hashes and release-specific approval receipts; no automatic service activation.
- Preserve loopback and school-LAN SSH listeners and existing host key identity.
- Replace authored container startup with Rust and retire obsolete Python/Go and
  one-time shell rollout/setup sources without changing the installed legacy tree.

- Extract CBGPU-gpu-daemon into an independently maintained organization repository.
- Preserve component authorship/history and add component-scoped CI and setup.
- See docs/MIGRATION.md for the original source commit and history mapping.
