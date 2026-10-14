# Spark Dev Bridge design

Status: implementation contract, 2026-10-13. Product name Spark Dev Bridge; repository nvidialink; CLI `sparkdev`.

## Evidence and boundaries

Read the Spark Link protocol, GPU placement document, SSH prototype and physical Spark probe before this design. Upstream reference is PhaseForge `ac30e04` on `claude/0.25-spark-p2`; the local checkout has advanced, so use git show/archive at the pinned commit. The protocol describes QR owner linking after SSH, endpoint-key authenticated iroh, short-lived approval tickets, and jobs.v2 content-addressed chunk transfer. GPU placement documentation still contains historical 0.21 capability statements: implementation at the pinned revision wins when they differ.

The new repo initially contains only README and MIT license. SSH was verified with BatchMode, StrictHostKeyChecking=yes, HostName 192.168.0.226 and HostKeyAlias 192.168.1.186 through tuningspork-spark. Spark reports aarch64, 20 cores and about 120 GiB RAM. Existing Spark Link and owner containers remain untouched. Local D: space is constrained; build output uses C:.

## Components

1. A Rust `sparkdev` binary/library owns CLI parsing, exact argv, project inventory, SHA-256 CAS upload, transport selection, local receipts and detached job interaction. Desktop calls the same binary with structured JSON; it does not interpret shell commands.
2. A Tauri desktop app provides SSH setup, connection health, QR pairing, linked Spark and job views. Its Windows executable carries a real multi-resolution ICO and Start-menu shortcut. Setup reuses existing SSH key references without reading or copying private key material.
3. A Python standard-library remote controller, separately installed under `~/sparkdev`, handles validated JSON RPC on stdin/stdout through SSH and the authenticated iroh adapter. A separate user systemd service schedules jobs and reconciles Docker containers after restart. SQLite transactions persist manifests, immutable job requests, fairness, cancellation and audit records. Docker owns detached workload lifetime; losing the connection or restarting the controller does not restart a job.
4. An independent Rust iroh bridge reuses pinned Spark Link protocol/pairing primitives with provenance, separate identity, ALPN, UDP/LAN ports and state. It never joins or controls the existing PhaseForge Spark Link service. Owner pairing starts only over pinned SSH. A QR opens the Spark LAN approval page; approval authorizes the desktop endpoint. Remote endpoints remain pinned and fail closed. Relay configuration awaits verified owner deployment details.

## Wire contract (version 1)

Each RPC is one JSON request and one JSON response. Requests include `v:1, op, ...`; responses include `ok` and structured error `code,message` on failure. Operations: `doctor`, `status`, `missing` (hash list), `put` (sha256, base64 chunk), `sync` (project, manifest), `run` (project, manifest_id, argv, runtime, client, request_id), `wait` (job, timeout capped 60), `logs` (job,since,limit capped 1 MiB), `cancel` (job). `run` persists request_id before container creation and rejects different content reusing a request_id. Request IDs are generated and saved locally before network submission; explicit `--request-id` allows recovery. Logs use byte offsets; responses carry `next_offset`, state and exit_code. Errors do not echo input contents. Remote helper command is fixed, never built from user argv.

Manifest entries: `{path,size,sha256,mode}`; project ID is SHA-256 of canonical local project path (distinct worktrees isolate caches). Manifest ID is SHA-256 of canonical manifest JSON. Chunk upload uses SHA-256-addressed objects, 1 MiB maximum chunks and per-file chunk lists for resumable large inputs. Server validates all hashes, sizes, paths, limits and materializes into immutable snapshots before execution. No tar extraction.

## Sync and secret policy

Inventory uses `git ls-files --cached --others --exclude-standard -z` within a Git project, with a safe walk fallback for a plain directory. Deny `.git`, target, node_modules, dist/build caches, symlinks/reparse points, paths outside root, secret directories, credential/key extensions and names. Scan file content for private-key headers and recognizable credential patterns; reject rather than print contents. Tracked ignored secrets are still rejected. Perform full policy validation before any upload. Persist CAS objects atomically; changed/deleted files produce a new immutable snapshot.

## Runtimes and resource isolation

Owner-installed registry contains only named Rust, Node, Python and optional CUDA runtimes pinned to immutable Docker digests/image IDs with provenance. Callers select a name, never an image, flags, host path or entrypoint. Use independent `sparkdev-*` container/network/volume names. Docker jobs: read-only root, nonroot user, cap-drop ALL, no-new-privileges, bounded tmpfs, pids >=512, memory and swap capped, CPU total <=16, nice priority 10, network none, no host/docker socket or other project mounts. Default memory headroom is at least 32 GiB. GPU jobs additionally require a MemAvailable watchdog because GB10 CUDA memory is not reliably bounded by cgroups.

Scheduler admission caps total 16 CPUs and 64 GiB across jobs, per-client concurrency 2 and CPU 8 (claude/codex/human); FIFO rotation between clients. Initial real validation uses smaller caps to avoid disrupting Claude. Per-project caches are locked for writing so different worktrees can run concurrently without cross-project pollution.

Dependency preparation is a separate fixed operation with lockfiles enforced: cargo fetch --locked, npm ci with package-lock, pip --require-hashes. Network is permitted only through an allowlisting HTTPS CONNECT proxy (crates.io/static.crates.io/index.crates.io, registry.npmjs.org, pypi.org/files.pythonhosted.org and specifically GitHub release asset hosts). Execution stays network-none. No unrestricted `--net` escape hatch. Until this path is verified, fetching fails closed and offline cached jobs remain possible.

## CLI and desktop

Commands run/sync/status/logs/cancel/doctor plus wait/setup/pair. Global --json, --client; run --detach, --runtime and --argv-json. Run passes through workload exit code; documented control failures use stable reserved codes. JSON mode returns structured messages; all captured output is saved in owner-local application state. `run --detach` returns after admission, `wait --timeout` stays bounded, logs are offset-idempotent. Pending cancel is persisted locally and retried on the next authenticated connection. SSH uses BatchMode, no TTY, no agent/password prompts and strict host pinning. LAN connection attempts are bounded; status reports the selected path and reason. An unconfigured or unpaired remote path is explicit, never substituted with insecure transport.

## Delivery and acceptance

Build source and tests first, then isolated physical Spark jobs and resilience tests, then installer/native inspection. Windows UI, PowerShell/cmd/Git Bash argv and Codex process/network permissions require local evidence. Full PhaseForge backend benchmarking uses a read-only source snapshot at the pinned commit, copied into this product's own workspace/cache, never the original working directory. Laptop baseline is run only through the shared build gate. Actual Claude Code and Codex concurrent invocations require receipts identifying the real callers; client labels alone are insufficient. Phone approval, hotspot and sleep tests require real user/device participation if tool control cannot provide them.

Coordinate with `Rebuild DeloresAI marketplace` for exact current manifest and scanning requirements. Listing must be non-featured. Do not publish placeholder artifacts or call an ingested artifact accepted before scanner/catalog/download evidence exists.

## Owner addendum: both computers and native agent tools

`run --where auto|laptop|spark` adds a durable laptop worker. Auto selects laptop for Windows targets/MSVC, NSIS, Windows-only build/test markers; Linux-capable jobs select Spark. Explicit placement is recorded. Local cargo goes through the existing unchanged buildlock. Scheduling independent jobs across machines does not split a compile or share target artifacts across OS/architecture.

The shared board (`status --all`) combines local and remote jobs and exposes client, placement, claim and cache state. A project/worktree claim prevents simultaneous builds of the same tree. Both runners persist job identity before launch; ambiguous launch is reconciled, never blindly repeated. Disconnected local work must disclose board availability and cannot steal a remote claim.

Result cache keys include tree content hash, exact argv and immutable runtime identity; include platform and any other output-affecting settings. Only fully succeeded jobs are eligible. `--no-cache` forces execution. Cache hits return the original complete logs/exit code with `cache_hit` and `cached_from`; no mutable toolchain may masquerade as a digest-pinned cache entry.

`sparkdev mcp` is a newline-delimited JSON-RPC stdio MCP server exposing run, wait, logs, status and cancel. It delegates to the same CLI core, binds the configured client identity, always detaches run and caps waits. Stdout contains protocol messages only. Ship `.mcp.json` and Codex TOML setup examples without secrets. Acceptance needs actual tool calls by real Claude Code and Codex sessions, not a simulated MCP client or a client label.

Verified relay clarification from PhaseForge release agent: the shipped option is explicit n0 public iroh relays, not a deployed owner VM. `relay.deloresai.com` is future/not deployed. Custom relay support remains configurable; private-VM/hotspot acceptance remains open until a real deployment/network is available.

## Product separation and license

Owner direction: cross-agent questions, delegation and reviews belong to a separate CodaClauda utility, not this implementation. Both may use Spark Dev Bridge CLI/MCP job contracts. PhaseForge remains featured; both utilities are non-featured. Both utilities have public repositories/listings and MIT first-party source. Ben's explicit license direction applies to his reused first-party code in these projects, with attribution and provenance; third-party licenses are preserved. No PhaseForge files or licensing are changed.
