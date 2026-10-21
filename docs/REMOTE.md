# Spark controller and runtime operations

The remote controller is a Python standard-library program installed separately at
`~/sparkdev/controller.py`. Pinned SSH invokes `python3 ~/sparkdev/controller.py rpc`
with one JSON request on stdin, then closes stdin. Responses are flat objects with
`ok:true`, or `ok:false,error:{code,message}`. Requests have `v:1`. The authenticated
iroh adapter invokes this same fixed command. This program opens no network listener.

`sparkdev-controller.service` is a user systemd scheduler. SQLite uses WAL and FULL
synchronous commits. Job IDs and request IDs are durable before Docker creation.
Container identity is the job ID plus its pinned runtime image ID and label.
Reconciliation distinguishes a positively missing container from unavailable Docker;
it never guesses that a Docker error means it should restart a workload. A running
container missing after reconciliation fails with `container_lost` instead of rerunning.

## Install and inspect

After verifying the SSH host pin, execute `remote/bootstrap.py` on the Spark and send
a JSON bundle on stdin:

```json
{"v":1,"files":{"controller.py":{"sha256":"...","data":"base64..."}},"build_runtime":true,"build_cuda":false,"relay_mode":"disabled"}
```

The abbreviated example must include these files: `controller.py`, `worker.py`,
`prepare.py`, `proxy.py`, `install-runtime.py`, `runtime/Dockerfile`,
`runtime/requirements.txt`, and `sparkdev-controller.service`. Optional files are
`bootstrap.py`, `install-cuda.py`, `runtime/Dockerfile.cuda`, `bin/sparkdev-link`, and
`sparkdev-link.service`. Each entry is SHA-256 checked before any installation write.
The closed filename list prevents path traversal. The installer only writes its own
`~/sparkdev` tree and named user service files. It retains changed old source files
under `~/sparkdev/install-backups`. It requires neither sudo nor a system service.
`relay_mode` may explicitly select `disabled` or `n0_public`; omitting it preserves
existing relay configuration, with disabled as the default for a new installation.

Runtime provisioning is detached through the separate `sparkdev-runtime-build`
user unit. The CPU Docker build uses the classic builder so its 2-CPU, 2-GiB limits
are honored. `~/sparkdev/runtime-build.json` and `runtime-build.log` give bounded
status and diagnostic output. Successful provisioning probes Rust, Node, Python,
NumPy and Pillow, then writes the immutable image ID into `~/sparkdev/runtimes.json`.
`build_cuda:true` explicitly opts into the optional CUDA image and 16-MiB GPU probe;
status is in `runtime-cuda-build.json`. CUDA is not admitted until that probe passes.
Docker's official NGC image retains its own licenses; DGXBridge does not
redistribute it as MIT software.

Examples on the Spark:

```sh
systemctl --user status sparkdev-controller.service --no-pager
journalctl --user -u sparkdev-controller.service --no-pager -n 20
printf '%s' '{"v":1,"op":"doctor"}' | python3 ~/sparkdev/controller.py rpc
```

The runtime registry is protected owner-local state, not repository configuration.
Allowed tokens are `rust`, `node`, `python`, `cuda`; every image must be a full
`sha256:` Docker image ID. Callers cannot choose images, Docker flags, host paths,
ports, mounts, or entrypoints. Registry image provenance is required. Runtime
images are only pulled/built during explicit owner provisioning, never job submission.

## Wire operations

| Operation | Fields after `v,op` | Result |
|---|---|---|
| `missing` | `hashes` (at most 10,000; `[]` probes) | `missing` hashes, sync `limits`, `storage` free and reserve bytes |
| `put` | `sha256,data` (base64, at most 1 MiB decoded) | accepted hash and size |
| `put_many` | `chunks:[{sha256,data}]` (128 objects / 12 MiB JSON body maximum) | ordered `accepted` objects and decoded byte count |
| `sync` | `project,manifest` | `manifest_id,files,bytes,project`, or `materializing` while the Spark assembles it |
| `run` | `project,manifest_id,argv,runtime,client,request_id,no_cache?` | job receipt |
| `prepare` | `project,manifest_id,runtime,client,request_id` | dependency job receipt |
| `wait` | `job,timeout?` (0â€“60 seconds) | current job receipt |
| `logs` | `job,since,limit?` (1 MiB maximum) | base64 `data`, display `text`, `next_offset`, receipt |
| `cancel` | `job` | durable cancellation receipt |
| `status` | `all?` | shared laptop/Spark board, scheduler heartbeat, resource `limits`, runtimes, observed `network` link, `storage`, `sync_limits`, `materializing` queue |
| `doctor` | none | status (including `network`) plus SQLite, Docker, Linux and memory checks |
| `resources` | `limits?` (complete owner policy) | effective policy, hardware bounds, persistence and application timing |
| `claim` | `project,client,request_id,job?,ttl?` | laptop `lease,job,expires_at` |
| `heartbeat` | `lease,job?,ttl?,state?,exit_code?,data?,since?` | renewed lease and next log offset |
| `release` | `lease` | terminal laptop claim |

A project ID is SHA-256 of the caller's canonical worktree path. Different worktrees
have independent caches. A manifest is a list of
`{path,size,sha256,mode,chunks:[{sha256,size}]}`. Entries are path-sorted, then encoded
as UTF-8 JSON with sorted object keys, no insignificant whitespace, and unescaped
Unicode. Its SHA-256 is the manifest ID. Files are rebuilt from verified chunk hashes
and whole-file hashes, without archive extraction. Symlinks, traversal, duplicate
case-insensitive names, excluded directories and credential patterns are refused.
A file may be up to 16 GiB and a project up to 32 GiB, of which files of 512 MiB or
less ("small" files) may total at most 8 GiB. At most 100,000 files are accepted.
These limits keep a full manifest well inside the 4 MiB paired-route frame; a 64 GiB
project waits for measured wired throughput and a prune operation.

## Limits handshake and snapshot assembly

A new CLI first sends `missing` with no hashes. The response carries `limits`
(`chunk`, `max_file`, `max_tree`, `max_small_tree`, `large_file`, `max_files`,
`max_request`, `put_many_body`, `missing_batch`) and `storage`
(`free_bytes`, `total_bytes`, `reserve_bytes`). A controller without `limits` is
treated as legacy: the CLI then refuses, before any upload, any file over 512 MiB or
project over 8 GiB, and asks the owner to update Spark setup. A different advertised
chunk size is refused. `missing` checks presence with `lstat` only (a regular object
of at most 1 MiB); it never re-reads objects. Assembly re-verifies every chunk.

`sync` validates the manifest, refuses `missing_chunks` before writing anything,
and queues the snapshot as `~/sparkdev/state/pending/<manifest>.json`. Snapshots that
need at most 512 MiB of new bytes are assembled inside the RPC, exactly as before, so
older CLIs and small projects see a synchronous sync. Larger ones are assembled by
one detached materializer (`controller.py materialize`, started with stdio closed in
its own session), which holds a single global lock and processes the queue oldest
first; at most four snapshots may be queued. A sync waits up to 20 seconds, then
answers `materializing` with progress ("X of Y MiB"); running the same command again
continues where it left off. The scheduler restarts the materializer whenever work is
queued and none holds the lock; its unit's `KillMode=process` keeps a running
materializer alive across scheduler restarts.

Assembly applies the same checks as before to every chunk: length, SHA-256 and the
credential scan with the previous chunk's 512-byte tail, then the whole-file hash. A
chunk whose content no longer matches its name is unlinked, and that sync answers
`hash_mismatch` once, so the CLI uploads it again. Files over 512 MiB are written once
to `files/<sha256>-<mode>`, fsynced, made read-only and hard-linked into each
snapshot that uses them (a fresh copy if the link count is exhausted). An interrupted
assembly resumes file by file: completed large files are kept, completed small files
are re-hashed because they were never fsynced, and a partial file restarts from zero.
A failure leaves a one-shot marker that the next sync returns (`insufficient_disk`
for a full disk) and removes the partial tree; `files/` is kept. No prune operation
exists yet.

`DISK_RESERVE` is 64 GiB of free space on the state filesystem. `put` and `put_many`
refuse with `insufficient_disk` when the incoming bytes would cut into it, `sync`
counts the bytes of snapshots already queued, and the scheduler leaves a job queued
while its work copy would cut into it (`status` shows `admission_blocked`). This also
blocks small syncs when the Spark has less than 64 GiB free; that is intentional,
because a full disk would also break running jobs, caches and the controller's own
database. Refusals and `status.storage` report free and reserve figures in GiB only.

Logs use byte offsets, not character counts. Base64 `data` is authoritative for
non-UTF-8 output; `text` is a lossy convenience field. Repeating an offset reads the
same bytes. Laptop heartbeat log writes permit exact retries, reject changed overlap,
and accept remaining log chunks after an identical terminal state was reported.

Successful identical remote jobs get a new audited receipt with `cache_hit:true`
and `cached_from`; logs refer to the completed original job. The key includes worktree,
manifest content/modes, exact argv, runtime image/provenance, fixed policy, resource
limits, and the supervisor content hash. Failed jobs never populate this cache.
`no_cache:true` executes again. Cached receipts are previous execution results, so
time-dependent commands should use this bypass. No artifact export is inferred.

## Network link report

`status`, and therefore `doctor`, carries `network`: an observation of which Spark
link carries this controller's traffic. It is observation only and never changes
route choice, admission or retry classification. The probe reads bounded sysfs and
procfs files (4 KiB per sysfs file, at most 1,024 interfaces, all classified before
the wired list is capped at 16). For Wi-Fi it runs `/usr/sbin/iw` or `/sbin/iw`
(`dev IF link`, `dev IF info`), else `/usr/bin/nmcli -t -e yes -f
ACTIVE,FREQ,CHAN,BANDWIDTH device wifi list ifname IF --rescan no`, with argv only,
`LC_ALL=C`, a fixed `PATH`, no stdin or stderr, 64 KiB of stdout, a one-second
timeout per call and a 2.5-second overall deadline. Signal falls back to
`/proc/net/wireless`. It never scans and never requests SSID or BSSID fields. Raw
tool output is parsed in memory and discarded, never logged. A single sanitizer
rebuilds the object from whitelisted, bounded fields, so it holds no SSID, BSSID,
MAC or IP address; the probe never reads an interface `address` file.

```json
{"v":1,"observed":true,"basis":"ssh_session",
 "active":{"interface":"wlP9s9","kind":"wifi","operstate":"up","speed_mbps":null},
 "underlay":null,
 "wired":[{"interface":"enP7s7","carrier":false,"operstate":"down","speed_mbps":null,"driver":"r8127","default_route":false}],
 "wired_up":0,"wired_lan_up":0,
 "wifi":{"interface":"wlP9s9","band":"2.4","channel":11,"frequency_mhz":2462,"width_mhz":20,
         "tx_bitrate_mbps":144.4,"rx_bitrate_mbps":72.2,"signal_dbm":-52,"links":1,"source":"iw"}}
```

- `basis` is `ssh_session` only when `SSH_CONNECTION` parses, its exact 4-tuple is an
  ESTABLISHED socket in `/proc/net/tcp` or `tcp6`, and a local interface owns the
  server address. Otherwise it is `default_route` (lowest-metric IPv4 default, else
  IPv6), which includes a stale `SSH_CONNECTION` inherited on the paired route. With
  no default route it is `unknown` and `active` is null.
- `kind` is `wired` (sysfs type 1 or 32), `wifi`, `virtual` (no `device`) or `unknown`.
  `active.egress_interface` appears only when the main-table route back to the client
  leaves through a different interface; it is not computed for a virtual owner, whose
  policy routing the main table does not show.
- `underlay` is set only when `active` is virtual: the first physical `lower_*` member
  with carrier, else the physical default-route interface.
- `wired_up` counts wired ports with carrier. `wired_lan_up` counts those that also
  have a default route, so a QSFP cluster cable alone is not a LAN link.
- `wifi` is reported only for an active or underlay Wi-Fi interface. `band` is `2.4`,
  `5`, `6`, `multi` (multi-link operation, with `links`; channel, frequency and width
  are then null) or null. `source` is `iw`, `nmcli`, `proc` (signal only) or null.
  Out-of-bounds or unknown values are null.
- `{"v":1,"observed":false,"basis":"unknown"}` means this controller could not observe
  its link (non-Linux host, unreadable sysfs or a failed probe); status still
  succeeds. A missing `network` key means an older controller, which a bridge repair
  replaces. `sparkdev devices` passes the object through as the Spark device's
  `network` and reports null for a missing key, an unreachable Spark or a version
  other than 1.
- Additive fields keep `v:1`; only a breaking change bumps `v`.

## Isolation, fairness and dependency access

Initial default jobs use 4 CPUs and 8 GiB, with at most 16 CPUs / 64 GiB admitted total,
two jobs / eight CPUs per client, and at least 32 GiB reserved memory headroom.
These defaults are hardware-bounded. The authenticated `resources` operation saves a complete validated policy in controller SQLite. It exposes per-job CPU/memory, total CPU/memory, per-client jobs/CPU and headroom. New requests and admissions use it; running jobs retain their recorded allocations and GPU headroom. Admission conservatively includes all reserved job memory. Preparation proxy overhead
also counts against quotas. The scheduler rotates FIFO admission between `claude`,
`codex`, and `human`; these labels are attribution, not proof which agent ran a job.
Each worktree has one active execution claim. Stale laptop claims remain blocking
until the client stops its process and releases/reports completion: sleep cannot
prove that a workload stopped exactly when a lease expired.

An owner-local, protected `~/sparkdev/limits.json` can grant a named worktree up to
8 CPUs / 32 GiB without changing defaults or global/client quotas. Its schema is
`{"v":1,"projects":{"PROJECT_SHA256":{"cpu":8,"memory_gb":32,"scratch_mb":4096}}}`. That legacy file is not edited by RPC. Once the owner saves the new resource policy, its per-job caps also clamp these legacy grants. The admission record, cache key, Docker limits and thread
environment all use the same resolved grant. Dependency preparation keeps its
conservative default limits. Owner configuration must never be committed.
Optional `scratch_mb` changes the job's `/tmp` tmpfs from its 512 MiB default;
the grant must be 128â€“4096 MiB and no more than half the job memory limit. Tmpfs
usage counts against that same memory limit. The resolved scratch limit is recorded
with the job, included in cache identity, and cannot be changed for existing jobs.

The container is nonroot, read-only-root, cap-drop ALL, no-new-privileges, PID limited,
CPU/memory/swap limited, nice 10, bounded tmpfs, and network-none. Host mounts contain
only this job's private work copy, output, supervisor and per-worktree/per-image caches.
The work copy is built from the manifest into `work.pending` and renamed into place:
small files are copied with modes 0644/0755, and each file over 512 MiB becomes a
symlink to `/sparkdev-source/<path>`. Only a manifest with large files adds one
read-only bind of its whole snapshot at `/sparkdev-source`; no project path text ever
appears in Docker arguments, the job cannot write or chmod a shared store inode, and a
small-only manifest gets byte-identical arguments to earlier releases. A work copy that
cannot be built fails the job with exit 125 (`insufficient_disk` or `work_copy_failed`).
The Docker socket, host home, Ollama, PhaseForge services and other caches are absent.
CUDA uses the same limits plus an active `MemAvailable` watchdog; on GB10 this is
necessary because CUDA unified-memory allocations can evade the cgroup RAM limit.
The watchdog depends on the running controller and is not a hardware memory quota.

Dependency preparation is a separate fixed job. Rust runs `cargo fetch --locked`,
Node runs `npm ci --ignore-scripts`, and Python requires `requirements.lock` and
`python3 -I -X utf8 -m pip --require-hashes --only-binary=:all:`. Lockfile hashes must remain unchanged.
Before pip runs, every line of a Python or CUDA lock must be a named requirement with
per-requirement hashes. Option lines (`-r`, `-e`, `--index-url`, `--no-binary` and the
rest), direct URLs, paths, and requirements whose text before any marker ends in an
archive suffix (`evil==1.0.tar.gz`, `x.whl[extra]`) are refused: pip would read those
as a local file or directory and build an sdist with the network attached. A coding
cookie other than UTF-8 on either of the first two lines is refused too, because pip
would decode the lock with it; UTF-8 mode keeps pip's decoding equal to this check's.
Npm installed trees and Python site packages are saved by lock hash; actual jobs
restore them while remaining offline. Npm lifecycle scripts are never given egress.
pip runs isolated, so a project file beside the lock (for example `json.py`) never
executes in the network-attached container, and every inherited `PIP_*` and `PYTHON*`
variable is replaced with fixed values. A site directory is built under a pending name
and replaces the previous one only after success. Jobs put prepared site directories
ahead of any `PYTHONPATH` the image already sets.

CUDA preparation (`--runtime cuda`) runs in the CUDA image under its own
`/usr/bin/python3` and installs `requirements-cuda.lock`: a hash-locked `--no-deps`
closure that excludes every package the CUDA image provides. The proxy remains the
Python image, so `status` lists `preparable_runtimes` only when the Python runtime is
installed. The preparation gets no GPU (`NVIDIA_VISIBLE_DEVICES=void`, no `--gpus`),
refuses to continue if any `/dev/nvidia*` device is visible, and is exempt from the
MemAvailable watchdog, which still stops CUDA runs. Before any download it applies the
lock-line rules above and refuses `torch`, `torchvision`, `torchaudio`, `triton`,
`pytorch-triton`, `cuda-toolkit`, every `nvidia-*` distribution except `nvidia-ml-py`
(CUDA 13 wheels such as `nvidia-cublas` carry no `-cuNN` suffix) and any distribution
already in the image; torch and the CUDA stack come only from the pinned image. After
the wheel-only install, metadata-only checks refuse duplicate image distributions or
modules, a staged top-level `nvidia` namespace, standard-library shadows,
`sitecustomize`/`usercustomize`, and requirements that are missing from both
the image and the lock or conflict with the lock. A conflict with the image's own
version only warns. No staged package code runs while the network is attached.

Upgrade by redeploying the controller through the setup bundle. The old CLI already
passes `--runtime cuda`; the old MCP server still refuses it. A new CLI or MCP server
against an old controller gets `invalid_request` and no job row; retry with the same
request ID after upgrading. Admitted jobs keep the `worker.py` and `prepare.py` copied
at admission. The new worker changes the run-cache key, so each runtime sees one cache
miss. A legacy partial Python environment stays loadable until the next `prepare`
rebuilds it. `remote/verify_cuda_dependencies.py` is the owner-run physical acceptance;
it uses only the CLI and requires the real `--client`.

Each preparation job gets an internal bridge with Docker's `isolated` gateway mode,
which removes the host bridge address. This prevents the host-service access that
plain `--internal` permits; see [Docker gateway modes](https://docs.docker.com/engine/network/port-publishing/#gateway-modes).
Its only egress path is a distinct fixed proxy container. The proxy accepts HTTPS
CONNECT on port 443 to exact package registry/release-asset hosts, rejects private
DNS resolutions and credentials, and connects the validated IP without re-resolving.
It rejects general GitHub browsing/git clone and unrestricted Internet access.
The scheduler removes only its own proxy/network when the preparation job terminates.

## Evidence boundaries and operational limits

Python tests use an in-memory Docker stand-in for lifecycle fault injection; they
prove controller decisions, not container isolation by themselves. Real runtime build
and probe receipts live on the Spark; CLI acceptance receipts identify actual job
IDs and observed outcomes. Phone-hotspot roaming, physical laptop sleep, real Claude
Code invocation, Windows installation behavior and Marketplace admission require
their own evidence. None follows from unit tests or a client label. Large-file
transfer (5 GB and 16 GiB files) has unit-test coverage only so far; owner acceptance
receipts recording MB/s, link type and seconds per GiB are still required.

Logs are limited to 256 MiB per job; exceeding the bound kills the workload with
exit 125. A job may write files of at most 4 GiB (`--ulimit fsize`), although it can
read synced inputs of up to 16 GiB. Jobs and source snapshots are retained
for inspection; no garbage collector currently deletes them automatically. The
controller refuses new jobs at 4096 retained records. Owners should monitor disk
space and archive old completed work deliberately. There is no general remote output
artifact download command yet; CLI logs and exit-code receipts are implemented.

The physical PhaseForge backend benchmark at pinned upstream `ac30e04` is recorded
in `remote/receipts/phaseforge-benchmark.json`; its corrected scratch-policy rerun
is in `phaseforge-scratch-corrected.json`. The latter reports 1969 passed, 18 failed,
75 ignored, including integration targets. See `NOTES_FOR_CLAUDE.md` for the
read-only ARM64 diagnosis and Windows-only failure. This is a complete test run,
not a passing upstream suite. The initial upload receipt describes a resumed
73.5 MB transfer; it must not be described as a fresh whole-tree upload timing.

The final Linux ARM64 CLI was built with embedded dependency metadata, passed all
23 CLI/MCP tests in the pinned Git-equipped runtime, and exercised real executable
`devices`, `plan`, MCP initialization, the eight-tool listing and MCP planning.
`remote/receipts/linux-arm64-cli-final.json` records the job, source hashes, binary
hash, exact-argument and no-plan-state-change checks. Its adjacent dependency JSON
contains the 45 packages actually embedded in the ELF binary. This container had
no owner credentials, so its device output correctly reports an unconfigured remote
route; authenticated Windows LAN and Linux package checks remain distinct evidence.
