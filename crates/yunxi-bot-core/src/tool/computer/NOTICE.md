`driver-0.36.0.json` contains the reviewed subset of Cua Driver 0.36.0 tool descriptions and input schemas, generated with the official release's `describe` command.

- Upstream: https://github.com/trycua/cua/tree/6d6c734c127025fb39ad5d585a10382808b39dc1/libs/cua-driver
- Release: `cua-driver-rs-v0.36.0`, MIT (see LICENSE.MIT).
- Copyright (c) 2025 Cua AI, Inc.
- The unmodified Driver subproject source is included at `third_party/cua/libs/cua-driver`; see `third_party/cua/README.yunxi.md` and `UPSTREAM.json` for provenance and licensing boundaries.
- Runtime is installed separately; no upstream binary or model weights are committed here. The source snapshot is not part of YunXi's Cargo workspace and does not enable additional Driver operations.
- Excludes arbitrary scripts, update/install, recording, history and session escalation tools. New upstream operations are never auto-registered.
