# DGXBridge design

Status: implementation contract, 2026-10-13. Product name DGXBridge; repository nvidialink; CLI `sparkdev`.

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

Owner-installed registry contains only named Rust, Node, Python and optional CUDA runtimes pinned to immutable Docker digests/image IDs with provenance. Callers select a name, never an image, flags, host path or entrypoint. Use independent `sparkdev-*` container/network/volume names. Docker jobs: read-only root, nonroot user, cap-drop ALL, no-new-privileges, bounded tmpfs, pids >=512, memory and swap capped, CPU total <=16, nice priority 10, network none, no host/docker socket or other project mounts. Default memory headroom is at least 32 GiB. GPU jobs additionally require a MemAvailable watchdog because GB10 CUDA memory is not reliably bounded by cgroups. CUDA dependency preparation uses the CUDA image without a GPU, so it is exempt; torch and the CUDA stack come only from the pinned image, and its `requirements-cuda.lock` may not duplicate or shadow image packages.

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

Owner direction: cross-agent questions, delegation and reviews belong to a separate CodaClauda utility, not this implementation. Both may use DGXBridge CLI/MCP job contracts. PhaseForge remains featured; both utilities are non-featured. Both utilities have public repositories/listings and MIT first-party source. Ben's explicit license direction applies to his reused first-party code in these projects, with attribution and provenance; third-party licenses are preserved. No PhaseForge files or licensing are changed.

## 0.2.0 owner-requested desktop release (2026-10-14)

This supersedes the beta's Marketplace-only/manual-update restriction. The owner requests one new release with an integrated Marketplace updater, configurable compute limits, connection modes and controls, PhaseForge connection visuals, and copyable agent prompts. The first installation must be run by ordinary Windows Explorer, outside Codex package virtualization. Preserve the existing owner configuration, pairing identity, logs and projects. Do not overwrite earlier releases or alter PhaseForge.

### Connection lifecycle

Persist `connection_mode` as `lan-only`, `remote-only`, or `both`, and `connection_enabled`. Migrate absent fields from the old `allow_remote` flag and connected default. LAN-only uses strict pinned SSH; Remote-only skips SSH and requires the existing authenticated paired transport; Both tries LAN first and falls back only on reachability failure, never host-key/authentication rejection. A paired route does not establish whether traffic is direct or relayed. Report observed routing and unavailability honestly.

Connect enables the saved route and checks it. Disconnect prevents new requests and admissions without deleting credentials/pairing or cancelling jobs. Already admitted independent jobs continue; an existing laptop worker retains its admitted transport snapshot for lease heartbeats. Fresh CLI/MCP processes honor the saved disconnected state. Configuration remains editable while disconnected; SSH bootstrap is a separate deliberate operation, not a prerequisite to every Connect click.

The Spark's network link report (`status.network`, the `devices` Spark `network`) is observation only: it never changes route choice, fallback, admission or retry classification, and it carries no SSID, BSSID, MAC or IP address.

### Resource controls

The resource pane exposes Spark per-job cores/memory, total CPU/memory, memory headroom, and per-client job/CPU quotas. The authenticated controller owns and persists the effective policy; clients read it back after saving. Detect physical CPU/memory bounds and validate all relationships. Defaults are starting values, not hidden immutable caps. Updated policy controls new requests/admissions; never kill running jobs to enforce a lower cap. Existing job resource snapshots remain inspectable. Disclose legacy project overrides and make explicitly saved per-job caps authoritative for subsequent jobs.

Laptop controls must reflect actual enforcement. Native Windows process trees require Job Object CPU/memory limits and controlled process launch; do not present a working memory/core cap when only scheduling or compiler parallelism is limited. Keep the unchanged local Cargo build gate. Separate laptop quotas from Docker/Spark policy and document platform limitations where enforcement is unavailable.

### Desktop and agent instructions

Reuse the actual PhaseForge `SparkLinkVisuals` topology and `sparkLinkParticles` renderer, separately from its QR animation, pinned with provenance. Show laptop, Spark, route, connected/disconnected/connecting/error states with accessible reduced-motion and hidden/offscreen handling. Retain job pagination. Group connection mode, saved SSH references, pairing, and independent bootstrap clearly; provide resource and update panes without a compressed layout.

Generate separate Claude Code and Codex copyable prompts and MCP snippets using the installed executable path, saved nonsecret alias/address/known-host alias/user/key-file reference, selected mode, current observed route and effective limits. Never include key contents, credentials, pairing tickets/codes or auth tokens. Label unknown/unavailable observations rather than inventing network facts. Include the stable request-ID, detach/wait/logs workflow and actual client label. The updater belongs in the navigation drawer, matching PhaseForge. Share the reviewed updater contract and implementation with ClaudaCoda, with separate product identities and release ownership; both utilities remain nonfeatured.

### Marketplace updater

Trace PhaseForge's shipped discovery, owner approval, download/revalidation and installer handoff. Adapt the current Marketplace native-update authorization contract (`docs/NATIVE-UPDATE-AUTHORIZATION.md`, schema 7) for Tauri rather than copying Electron cookie/session behavior blindly. Use the app's exact accepted stable artifact and S256 proof; browser owner approval; proof-bound poll/download/revalidate; distinct trusted acquisition and review authority maps; exact app/platform/version/source/hash/size/preservation checks. Fail closed on stale/revoked/untrusted/mismatched metadata and never substitute public GitHub downloads for an authorized update. Installation is an explicit user action and preserves app state. No secrets in receipts or frontend diagnostics.

The updater's eligible channel requires a stable release, so target version 0.2.0, preserving honest feature/verification limitations. Verify source tests, real Spark policy/routing, installed Windows controls and the actual updater authorization/download lifecycle separately. Marketplace admission and a displayed update button are not update-installation acceptance. Publish immutable artifacts, submit the manifest through the normal flow, then install the first updater-capable version from normal Explorer and verify the physical owner install and shortcuts.

## 0.2.1 packaging and resource usability

Keep 0.2.0 and its rejection findings immutable. Package remote scripts and the ARM64 executable as ordinary inspectable files beside a bounded manifest of names, sizes and SHA-256 digests. Validate the closed file set, regular-file paths, bounds and hashes before constructing the existing authenticated SSH bootstrap envelope in memory. Do not use scanner exclusions or new opaque encodings. Scan the exact packaged payload as well as the tagged source before publication.

Resource presets edit a draft and require an explicit Save. Offer Ollama first, Balanced and Builds first; clamp recommendations to observed hardware and preserve advanced numeric controls. Bars describe configured budgets versus physical capacity, not unmeasured live usage. Explain per-job, per-agent and total budget relationships before saving. Preserve unsaved drafts during refresh. Dependency preparation must reserve its proxy CPU inside the configured admission budget.

Agent setup uses verified ordinary physical paths for unpackaged Claude Code and the verified loopback alias when needed by packaged Codex. The preferences response supplies per-agent paths; never hardcode owner paths or require the admin-share alias for every client. Testing a wired address must retain strict SSH host pinning and authentication. A failed wired probe must not replace a working route.
