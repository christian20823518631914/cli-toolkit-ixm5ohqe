# Aegis Core

Dependency-light Rust execution core for short-lived, multi-language workloads.

The first release supports C, C++, Python, JavaScript, TypeScript, and Go through one CLI and one JSON API. It uses fixed command vectors, isolated temporary workspaces, process groups, wall-clock deadlines, output caps, rlimits where runtime-compatible, bounded concurrency, and BLAKE3 compiled-artifact caching.

## Current scope

This repository is the deployable process backend and control-path prototype. It is designed so the process backend can be replaced by a Firecracker/Jailer backend without changing the API or runtime contract.

The current backend is suitable for trusted code, local development, CI, and the bundled sample-only public demo. It is **not** a hostile multi-tenant security boundary because processes still share the host kernel. See [the threat model](docs/security.md).

## First-launch languages

| ID | Runtime | Mode |
| --- | --- | --- |
| `c` | GCC/Clang-compatible `cc`, C17 | compiled + cached |
| `cpp` | `c++`, C++20 | compiled + cached |
| `python` | Python 3 | interpreted |
| `javascript` | Node.js | interpreted |
| `typescript` | built-in low-latency type erasure + Node.js | transpiled |
| `go` | Go | compiled + cached |

TypeScript Lite intentionally targets small snippets. Production project-level TypeScript should install a runtime plugin backed by SWC, esbuild, or `tsc`.

Additional languages are data-driven. See [runtime manifests](docs/runtime-manifests.md); adding one does not require modifying the Rust core.

## Build

```bash
cargo build --release
```

Toolchains required to execute every bundled runtime:

```bash
sudo apt-get install -y build-essential clang golang-go python3 nodejs
```

## CLI

```bash
./target/release/aegis-core list
./target/release/aegis-core --runtime-dir custom-runtimes list
./target/release/aegis-core run --language c --file examples/hello.c --json
./target/release/aegis-core run --language python --file examples/hello.py
```

Compiled C, C++, and Go artifacts are cached in `.aegis-cache`. Override the location with `AEGIS_CACHE_DIR`.

## API and interactive demo

```bash
./target/release/aegis-core serve --addr 127.0.0.1:8080
```

Endpoints:

- `GET /healthz`
- `GET /v1/languages`
- `POST /v1/run`

```json
{
  "language": "c",
  "code": "#include <stdio.h>\nint main(void){puts(\"hello\");}",
  "stdin": ""
}
```

For a public demo, use sample-only mode:

```bash
./target/release/aegis-core serve --addr 0.0.0.0:8080 --demo-mode
```

In this mode, modified or arbitrary source is rejected. The browser UI is served at `/`.

Generate a standalone copy of the UI:

```bash
./target/release/aegis-core demo --out demo/index.html
```

## Performance design

- One synchronous request path; no database, Redis, worker queue, container runtime, or shell invocation.
- One compiler slot and two runner slots by default, matching the target 2-vCPU node.
- BLAKE3 cache keys include language, source, toolchain identity, and fixed arguments.
- Compiler and program processes receive fresh workspaces and are killed as process groups on timeout.
- Output is drained without unbounded buffering and truncated at 64 KiB per stream.
- Node receives an explicit 128 MiB old-space cap; C-family processes use address-space rlimits.

Run the local benchmark:

```bash
scripts/benchmark.sh
```

## Quality gates

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

The CI workflow also executes all six bundled languages.
