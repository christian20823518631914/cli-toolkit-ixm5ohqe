# Security model

## Protected assets

- API process availability within configured limits
- Files outside the per-job workspace
- Output memory in the Rust control process
- Subsequent jobs from stale process groups

## Process backend controls

- No shell command construction
- Fixed executable and argument vectors
- Empty environment with a fixed `PATH`
- Random per-job workspace
- Source and stdin capped at 64 KiB
- stdout and stderr capped at 64 KiB each
- Wall-clock timeout and process-group `SIGKILL`
- CPU, file-size, core-dump, and compatible address-space rlimits
- Bounded compiler and runner concurrency
- Sample-hash allowlist in public demo mode

`RLIMIT_NPROC` is intentionally not used in the local backend because Linux accounts it per real UID, which would couple unrelated jobs and host processes. The production backend must enforce process counts with a per-VM cgroup.

## Explicit non-goals of the process backend

It does not prevent a deliberately hostile program from attacking the shared host kernel or using permitted networking. Do not expose unrestricted `/v1/run` to untrusted internet users.

## Production isolation boundary

The production backend should preserve the same `RunRequest`/`RunResponse` contract and replace process execution with:

1. Rust gateway without `/dev/kvm`
2. Privilege-separated Rust executor
3. Firecracker launched through Jailer
4. Fresh compiler snapshot VM for compiled languages
5. Fresh runner snapshot VM for every execution
6. vsock-only source, artifact, stdin, and output transport
7. No guest network device
8. cgroup v2 limits around every VMM
9. guest-internal UID drop, seccomp, Landlock, rlimits, and process cgroup
10. unconditional VM destruction after a job

Compiler VMs and runner VMs must never be reused after processing user-controlled bytes. Cache artifacts as opaque bytes and execute cache hits only inside a fresh runner VM.

## Public demo policy

The online demo must start with `--demo-mode`. It accepts only the exact six source samples compiled into the binary, preventing the demo endpoint from becoming a public arbitrary-code-execution service.
