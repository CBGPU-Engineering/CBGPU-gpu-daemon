# CBGPU GPU Daemon

[![CI](https://github.com/CBGPU-Engineering/CBGPU-gpu-daemon/actions/workflows/ci.yml/badge.svg)](https://github.com/CBGPU-Engineering/CBGPU-gpu-daemon/actions/workflows/ci.yml)
[![Python](https://img.shields.io/badge/Python-Controller-3776AB?logo=python)](https://www.python.org)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-D22128)](LICENSE)

The host-side GPU reservation controller, managed container lifecycle and SSH
gateway for CBGPU. This is a privileged infrastructure component, not the web UI.

## Tests

```bash
git clone https://github.com/CBGPU-Engineering/CBGPU-gpu-daemon.git
cd CBGPU-gpu-daemon
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
.venv/bin/python -m unittest discover -s tests
cd gpu_ssh/gateway && go test ./...
```

Go 1.26 is required by the SSH gateway module. Python tests use mocks and temporary
keys. They do not start, reset or remove production GPU containers.

## Installation

Use [CBGPU-platform](https://github.com/CBGPU-Engineering/CBGPU-platform) for the existing `Daemon/` integration
path. Review [installation details](docs/INSTALLATION.md) and configure your own
backend URL, keys, GPU/device mappings, storage, bind addresses and service units.
Do not run setup scripts or the controller on an arbitrary workstation just to
try the project; use the web demo instead. Runtime service-role keys and host SSH
keys stay outside Git. Existing public client defaults in the historical sample
are not credentials for another installation.

Original code is [Apache-2.0](LICENSE); see [history provenance](docs/MIGRATION.md).
