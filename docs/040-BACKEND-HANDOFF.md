# DGXBridge 0.4.0 backend handoff

Historical backend handoff, 2026-10-20; integrated into the complete 0.4.1 candidate on 2026-10-21. The owner approved job internet, shared downloads, model servers and the Spark half of fastest-route together in 0.4.0; this supersedes the older specs' proposed deferrals. **This is not a deployment, release, installed-app or Marketplace acceptance receipt.**

## Scope and integration ownership

| Area | Backend implementation | Remaining integration |
| --- | --- | --- |
| Job internet | Default-on public HTTP/HTTPS proxy, per-job internal networks, private/host/metadata guards, owner off switch, bounded counters, separate locked preparation allowlist | Real Docker DNS/IPv6, download and off-switch checks; desktop policy/status presentation |
| Saved downloads | Shared `/cache`, private token/code-cache/temp paths, separate preparation cache, idle-only bounded cleanup, saved usage reporting | Physical cache reuse, permissions, atime and cleanup receipts; owner controls in UI |
| Model servers | Durable `serve`/`call`/`reply`, named lifecycle, quotas, spool validation, loopback forwarding, CLI and MCP integration | Real loaded-model calls and lifecycle checks; desktop service presentation |
| Fastest-route | Authenticated `lan_routes`, physical RFC1918 candidates, salted change epoch, address-free `status.network` | **Claude owns laptop learning, candidate ranking/probing, strict-pin route choice, fallback and UI.** The Spark response alone does not switch routes. |
| Scheduler | Nonblocking file opens, per-row fault isolation, bounded failed launches, Docker-outage admission stop, newest-first memory watchdog, all active rows in status, runtime-refresh gate | Physical resilience and GB10 memory checks |

Build new **`sparkdev` and `sparkdev-link` binaries for the 0.4.0 package**, including the Windows CLI/adapter and matching ARM64 components used by the installer. Updating Python alone does not add CLI/MCP commands or the paired timeout clamp. Package the new `model_server.py` and `model_cache.py` with the matching controller, worker and proxy; regenerate and verify the closed payload manifest. Runtime images that embed the worker must be refreshed through the idle-only refresh path, preserving registrations until replacement succeeds. Do not reuse old binaries as acceptance evidence.

## JSON and CLI contract

Requests use `v:1`; normal RPC responses add `ok:true` to the operation result. Errors are `{ok:false,code,message}`. Fields below are names and types, not captured owner configuration. Clients must tolerate additive response fields and unavailable/null observations.

### Internet and saved downloads

- `resources` accepts optional `internet: boolean` independently of numeric `limits`. CLI: `sparkdev --client human --json resources --internet on|off`. Approval of the release default does not authorize later policy changes by agents.
- `status.internet` and `devices.devices[id=spark].internet` contain `{enabled:boolean,saved:boolean,available:boolean,blocks:string,applies:string}`. `available` means the Python helper runtime is registered, not that live public access succeeded.
- Job rows contain `internet: "on"|"off"|"cut_by_owner"|"unavailable"|"package_registries_only"|null`, `internet_reason:string|null`, and `internet_use:object|null`. Saved counter objects have exactly `connections`, `http_requests`, `bytes_down`, `bytes_up`, `blocked_private`, `refused_invalid`, `failed_connect` as nonnegative integers; they contain no URLs. Do not present these as live throughput.
- `downloads` request: `{v:1,op:"downloads",client:"codex"}`; optional `keep_gb` or `free_up` requires `client:"human"`. The response wraps `downloads`. CLI: `downloads`, `downloads --keep-gb N`, `downloads --free-up`. No cache-clearing MCP tool is exposed. Client labels provide attribution, not a second authentication boundary.
- `status.downloads` and the Spark device's `downloads` use `{v:1,folder,files_folder,used_bytes,keep_bytes,free_bytes,floor_bytes,measured_at,over_limit,clear_pending,atime,items,last_cleanup}`. `folder` is `/cache`, `files_folder` is `/cache/files`; unmeasured values may be null. `items` is bounded to 20. Read status without triggering a recursive cache scan.
- All owner jobs can modify shared downloads, including offline jobs. Keep credentials and generated executable caches private. `--no-cache` skips successful-result reuse but retains downloads. Result keys segregate internet policy; internet-enabled results can still be reused.
- Clearing queues while busy. Cleanup requires an idle scheduler and successful Docker guard; recent content can keep usage above the budget. A displayed limit is not a promise that space has already been reclaimed.

### Named services

```text
sparkdev --client claude --json serve DIR --name NAME --memory 16 --runtime cuda --request-id START_ID -- python3 server.py
sparkdev --client claude --json call NAME --input request.json --content-type application/json --request-id CALL_ID --save response.wav
sparkdev --client claude --json wait NAME --timeout 20
sparkdev --client claude --json logs NAME --since 0
sparkdev --client claude --json cancel NAME
```

Use the actual calling agent's client label. `serve` is Spark-only, returns immediately after submission, and supports `--port` (default 8000), `--memory` (GiB, default 16), `--idle-minutes` (default 30), `--runtime`, `--replace`, `--request-id` and exact `--argv-json` input. Runtime defaults to provisioned CUDA, otherwise Python. `--body-json TEXT` is the documented inline body option for `call`; **global `--json` controls output**. `--body-json` and `--input` are mutually exclusive. CLI file output never overwrites a different existing result.

Raw operations:

| Operation | Required fields beyond `v,op` | Optional fields |
| --- | --- | --- |
| `serve` | `project`, `manifest_id` (SHA-256 strings), `argv` (string array), `client`, `request_id`, `name` | `runtime`, `port`, `memory_gb`, `idle_minutes`, `replace` |
| `call` | `server`, `client`, `request_id` | `body` (base64), `content_type`, `path`, `timeout` (integer 0–60) |
| `reply` | `request_id`, `since` (byte offset) | `limit` (1–2 MiB) |
| `cancel` by name | `name` | None; do not combine with `job` |

`serve` returns the ordinary job shape plus `kind:"service"`, `name`, `port`, `idle_seconds`, `ready_at`, `last_activity`, `stop_reason`, `service_state`. CLI adds its retained `request_id` and transport observation. `service_state` is `loading`, `ready`, `waiting for memory`, or the terminal job state. The queued label alone does not establish that memory is the only admission constraint; inspect admission details.

Raw `call`/`reply` results contain:

```text
{request_id,server,job,state,status,content_type,size,sha256,data,since,next_offset,message}
```

`state` is `loading|waiting|done|failed|stopped|expired`; HTTP `status`, content type, size and hash are null until available. `data` is base64, at most 2 MiB per response. CLI fetches remaining chunks, verifies identity/size/SHA-256, removes `data` and adds `file`, `hash_verified:true`, optional `saved_as`, optional parsed `json` up to 64 KiB, and `process_exit_code` (0 for HTTP 2xx, otherwise 1). A handled RPC/tool operation is not automatically a successful inference. Inspect state and HTTP/process status.

Important integration rules:

- Service names match `^[a-z][a-z0-9-]{0,23}$` and cannot start with `laptop`; call IDs match `^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$`. Persist IDs before submission and retry uncertain calls with the identical payload and ID. Files expire before the 24-hour idempotency record; expiry must not silently rerun inference.
- CLI call polling defaults to 300 seconds; MCP defaults/caps the loop at 45 seconds. Connection setup and individual transport overhead are additional. A waiting response is resumable. The paired adapter clamps every timeout-bearing RPC to at most two seconds, including after LAN-to-paired fallback.
- MCP has 12 tools, adding `serve` and `call`. MCP `serve` uses `memory_gb` while CLI uses `--memory`; MCP `call` uses `body_json`. MCP binds the launch client's identity and marks nonzero `process_exit_code` as an error.
- Logs/wait resolve a name to its active service, otherwise newest matching service. **Name cancellation is live-only and never enters pending cancellations**; job-ID cancellation remains durable. MCP cancel accepts exactly one of `job` or `name` and maps a name to positional `cancel NAME`.
- Same active definition reuses its service. Automatic replacement requires the same client and project; otherwise explicit `replace` is needed. A new request-ID alias remains bound to the original service after it stops.
- At most two services, each reserving two CPUs and 1–48 GiB within the total owner budget, default 16 GiB. Services bypass batch per-job/client caps and worktree claims, but still consume total reservation/headroom. Ready-service memory is not subtracted twice from measured available memory.
- Input is at most 1 MiB; output 256 MiB; 32 waiting calls; reply storage 2 GiB or 10,000 entries. Payloads become eligible for cleanup at the earlier of completed delivery plus 10 minutes or creation plus one hour; call identity metadata becomes eligible 24 hours after creation. Deletion requires successful controller housekeeping; delays or errors extend retention. Laptop replies are retained locally without automatic deletion.
- Only recent idle/lifetime/internet-off stops may wake. Manual stop, crash, replacement or watchdog failure must not wake. Waiting calls retarget once after lifetime expiry, never to replacement code; a busy runtime-refresh gate preserves waiting for a later retry.
- No published port or host network: calls use authenticated RPC, read-only requests and a separate writable reply spool. The controller copies validated replies into unmounted storage and hashes them itself. A worker startup/lifetime backstop and log-draining cap remain necessary even if the scheduler is unavailable.

### Spark route discovery

`{v:1,op:"lan_routes"}` returns `{v:1,routes:[{kind,ipv4,prefix,speed_mbps}],shared_network,epoch?}` plus the RPC envelope. Routes are deduplicated physical wired/Wi-Fi RFC1918 candidates, at most eight, prefix 8–30, wired fastest first. Unknown speed is null. `epoch`, when present, is 16 lowercase hex characters derived from a private owner salt. A probe/salt error may omit it.

Only `network.lan_epoch` enters status/doctor/devices' network object; private candidate addresses and the salt must not enter job configuration, audit reports or UI telemetry. Claude's laptop implementation must validate candidates and authenticate the pinned SSH identity before use. A route reported as wired does not prove cable throughput, particularly when Wi-Fi shares the subnet. LAN route discovery is not the complete host-address deny list for the internet guard.

## Verified source checks

The final Python suites passed: **Windows 260 tests, 11 skipped; WSL Linux 260 tests, 10 skipped**. These are source/fixture tests, including fake-Docker lifecycle and filesystem fault injection; WSL is not the physical ARM64 Spark. The model-server/service integration subset separately ran 33 tests with one POSIX-only Windows skip and all remaining tests passing. The bundle/manifest script suite ran 13 tests with one skip and all remaining tests passing.

Marketplace's source review confirmed the corrected synthetic manifest passes the actual schema with zero errors and no artwork-shape issues. This checks the disclosure shape only, not real release assets or Marketplace admission. The reviewed contract SHA-256 is `6a4f72cec3249fd855fb06c6e49683ad895b110ba17a41b02dafa37113028540`; exact final source, asset and media validation is still required on the integrated release.

**Rust workspace tests and formatting: pending root's final gated result.** Fill in the exact result before release handoff; no Rust total is claimed here.

## Physical Spark acceptance checklist for Claude

Capture version/source/payload hashes, real job/call IDs, observed route, elapsed times and final states in credential-free receipts. Coordinate shared resources before starting; preserve unrelated containers, owner policy/state and all PhaseForge services. Use isolated DGXBridge fixtures and the actual caller identity.

- [ ] Deploy matching candidate scripts, CLI/link binaries and refreshed worker runtimes through the normal bridge setup. Verify existing owner configuration, pairing and runtime registrations survive; verify upgrade errors from a deliberately old controller are clear.
- [ ] Prove public HTTP and HTTPS work while home/private, loopback, link-local/metadata, every Spark host address and connected-subnet targets are blocked. Check redirects/re-resolution and actual Docker DNS/IPv6 behavior. If isolation cannot be established, confirm explicit offline status rather than unguarded access.
- [ ] Download more than 1 GiB and sustain a permitted transfer beyond 900 seconds, recording bounded helper resources and counters. Exercise the owner's authorized off-switch test; verify ongoing access is cut within one scheduler tick or the verified service is stopped safely. Restore any temporarily changed policy with readback.
- [ ] Use two separate worktrees/jobs to prove shared model/download reuse, including a `--no-cache` rerun. Verify token files and executable extension caches are private, locked preparation remains isolated, and runtime dependencies remain usable. Measure real bind-mount atime; test deferred clearing/eviction only in an authorized isolated cache fixture.
- [ ] Start a real small model HTTP service. Measure cold/first and repeated calls over pinned LAN and paired routes; verify response hashes, HTTP failure exits and exact argv from PowerShell, cmd and Git Bash. An authenticated paired route alone does not prove hotspot or relay use.
- [ ] Interrupt a call connection and restart only the candidate controller: retry the retained ID, confirm no duplicate request, complete response and correct terminal state. Test 32-call pressure, no idle stop while waiting, idle wake, explicit-stop no-wake, replacement isolation and lifetime retarget once using bounded fixtures.
- [ ] Check model-server admission above the batch per-job cap, aggregate caps/headroom, third-server refusal, no public/listening host port and no cross-job service access. Verify hostile reply files and an unreadable reply directory cannot stall other jobs.
- [ ] Exercise one missing-image/failed-create row alongside healthy work, a Docker outage and scheduler restart: bounded `launch_failed`, no unsafe admission, all active rows still visible. Test disk-floor stops without filling the owner's disk, using isolated fault injection.
- [ ] Run the agreed bounded GB10 memory test (8 GiB container limit, attempted 16 GiB GPU allocation) only with coordinated headroom and monitoring. Record actual cgroup behavior and newest-first watchdog response; do not infer hard GPU isolation from configured limits.
- [ ] After Claude's laptop route integration, check cable insertion/removal, strict-pin rejection and paired fallback; measure 1 GiB sync over Wi-Fi and cable with Wi-Fi on/off. Record OpenSSH version and observed route, not an assumed speed improvement.
- [ ] Obtain real Claude Code and Codex MCP receipts for candidate `serve`/`call` and existing job tools. Build/package, installed Windows UI/updater preservation, phone/hotspot tests and Marketplace admission each need separate receipts; none is completed by this document.

Detailed references: [design](DESIGN.md), [CLI](CLI.md), [MCP](MCP.md), [servers](SERVERS.md), [controller and trust boundaries](REMOTE.md), [privacy](PRIVACY.md).
