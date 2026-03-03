# pyroscope-rs

We are currently working on a new profiler implementation. The design document is in the adjacent `pyroscope-rs-design-docs/` folder (`design.md`).

## Contribution Workflow

- All code goes to: `https://github.com/korniltsev-grafanista-yolo-vibecoder239/pyroscope-rs`
- Pull requests must target: `https://github.com/korniltsev-grafanista-yolo-vibecoder239/pyroscope-rs` (not the Grafana fork)
- Every change goes to the `yoloprof` branch, based off `main` if it doesn't already exist

## Issue Tracking

Use the Vibe Kanban MCP server for all issue tracking. Do **not** use markdown files or git for task management.

# Build & Packaging

When adding new workspace crates or source directories needed for Rust compilation, update ALL of these:
- `MANIFEST.in` — include Cargo.toml and source files so Python sdist contains them
- `docker/wheel.Dockerfile` — ADD the directory for Python manylinux wheel builds
- `docker/wheel-musllinux.Dockerfile` — ADD the directory for Python musllinux wheel builds
- `docker/gem.Dockerfile` — ADD the directory for Ruby gem builds

All three Dockerfiles and the MANIFEST.in must stay in sync with workspace members in the root `Cargo.toml`.
