# Spark controller and runtime operations

The remote controller is a Python standard-library program installed separately at
`~/sparkdev/controller.py`. Pinned SSH invokes `python3 ~/sparkdev/controller.py rpc`
with one JSON request on stdin, then closes stdin. Responses are flat objects with
`ok:true`, or `ok:false,error:{code,message}`. Requests have `v:1`. The authenticated
iroh adapter invokes this same fixed command. This program opens no network listener.

Public job internet, shared downloads and named services below refer to the 0.4.1
development candidate. They need the matching Spark redeploy; source documentation
is not evidence that a new installed or published release has these features.

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
published ports, mounts, or runtime entrypoints. Named services select only their
own container's loopback HTTP port. Registry image provenance is required. Runtime
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
| `cancel` | `job`, or live service `name` | durable job cancellation; name cancellation is never queued offline |
| `status` | `all?` | shared laptop/Spark board, scheduler heartbeat, resource `limits`, runtimes, observed `network` link, `storage`, `sync_limits`, `materializing` queue |
| `doctor` | none | status (including `network`) plus SQLite, Docker, Linux and memory checks |
| `resources` | `limits?` (complete owner policy), `internet?` (boolean) | effective policy, hardware bounds, persistence and application timing |
| `downloads` | `client,keep_gb?,free_up?` | `downloads` saved usage and owner-change/clear-pending state |
| `serve` | `project,manifest_id,argv,client,request_id,name,port?,memory_gb?,runtime?,idle_minutes?,replace?` | named service job receipt |
| `call` | `server,client,request_id,body?,content_type?,path?,timeout?` | durable call state, HTTP status, checked reply metadata and first body chunk |
| `reply` | `request_id,since,limit?` (2 MiB maximum) | next reply bytes and offset |
| `claim` | `project,client,request_id,job?,ttl?` | laptop `lease,job,expires_at` |
| `heartbeat` | `lease,job?,ttl?,state?,exit_code?,data?,since?` | renewed lease and next log offset |
| `release` | `lease` | terminal laptop claim |

A project ID is SHA-256 of the caller's canonical worktree path. Different worktrees
have independent build caches and prepared environments. The 0.4.1 model/package
download cache is shared by all worktrees in one owner trust domain. A manifest is a list of
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

`sync` validates the manifest and counts what the snapshot still needs: the bytes of
absent chunks (each distinct chunk once), the new bytes assembly will write, and the
small files kept from an interrupted pass, which assembly re-hashes. Before anything is
uploaded it refuses `insufficient_disk` when the absent and new bytes plus every queued
snapshot's outstanding bytes would cut into the disk reserve, then `missing_chunks`
when chunks are absent. Otherwise it queues the snapshot as
`~/sparkdev/state/pending/<manifest>.json`; reading the queue, both checks and writing
the record happen under one short `pending/.queue.lock`, so concurrent syncs cannot
both pass them against the same queue. A snapshot whose new and re-hashed bytes total
at most 512 MiB is assembled inside the RPC, exactly as before, even while a background
build holds the global lock, and the queue cap never applies to it, so older CLIs and
small projects see a synchronous sync. Larger ones are assembled by one detached
materializer (`controller.py materialize`, started with stdio closed in its own
session), which holds a single global lock and processes the queue oldest first; at
most four such snapshots may be queued. Every builder, in an RPC or in the background,
holds an exclusive lock on the snapshot's queue record, so one snapshot is never built
twice at once. A sync waits until 8 seconds after its controller process started
(reading the resent manifest counts), then answers `materializing` with progress
("X of Y MiB"). That leaves the rest of the CLI's fixed 30-second sync timeout for
connection setup and the manifest upload: on the paired route, up to 12 seconds for the
sidecar to connect and 10 for the Spark daemon to read the frame. Running the same command again
continues where it left off. A sync never forks the materializer while the scheduler
is alive: the scheduler's next tick, about a second later, starts it whenever work is
queued and none holds the lock, so it runs inside `sparkdev-controller.service`, whose
`KillMode=process` keeps it alive across scheduler restarts. A new session is not a
new cgroup, so a materializer forked by an RPC would belong to whatever served that
RPC (`sparkdev-link.service` on the paired route) and die when that unit restarts.
Only when the scheduler's heartbeat is at least 15 seconds old does a sync start the
materializer itself; if that serving unit then restarts, the partial file restarts
from zero when the scheduler resumes the queue.

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

`DISK_RESERVE` is 64 GiB of free space on the state filesystem. Bytes that queued
snapshots will still write are already promised, so every guard counts them: `put` and
`put_many` refuse with `insufficient_disk` when the incoming bytes would cut into the
reserve, `sync` refuses before any upload as above, and the scheduler leaves a job
queued while its work copy would cut into it (`status` shows `admission_blocked`).
Assembly checks again before each file it writes and fails the snapshot with
`insufficient_disk` instead of crossing the reserve, because uploads and jobs may have
used space since the snapshot was queued. This also
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
timeout per call and a 2.5-second overall deadline. A tool still running at its timeout
is killed, given 0.2 seconds to exit, then abandoned, because a tool wedged in a
driver can ignore SIGKILL. The whole probe, sysfs reads included, runs on its own
thread for at most 3 seconds; one that has not returned by then reports
`observed:false`, so a wedged driver cannot hold `status` past the CLI's timeout. Signal falls back to
`/proc/net/wireless`. It never scans and never requests SSID or BSSID fields. Raw
tool output is parsed in memory and discarded, never logged. A single sanitizer
rebuilds the object from whitelisted, bounded fields, so it holds no SSID, BSSID,
MAC or IP address; the probe never reads an interface `address` file. An interface
name derived from a MAC address (`enx<MAC>`, `wlx<MAC>`, `wwx`, `ibx`, `slx`: Ubuntu
names USB network adapters this way) is reported as `usb-ethernet-N`, `usb-wifi-N`,
`usb-wwan-N`, `usb-ib-N` or `usb-slip-N`, numbered in sorted name order and identical in
every field; the real name is used only in the probe tools' argv.

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
  have a default route, so a QSFP cluster cable alone is not a LAN link. A port's
  `default_route` is true when the port itself, or a bridge, bond or VLAN master stacked
  on it (followed through its `upper_*` links, at most three layers), holds a default.
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
These defaults are hardware-bounded. The authenticated `resources` operation saves a complete validated policy in controller SQLite. It exposes per-job CPU/memory, total CPU/memory, per-client jobs/CPU and headroom. New requests and admissions use it; running jobs retain their recorded allocations and GPU headroom. Aggregate admission includes all reserved job memory; the live headroom calculation treats already-ready services separately as described below. Preparation proxy overhead
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
CPU/memory/swap limited, nice 10 and bounded tmpfs. Offline jobs use network-none;
internet-enabled jobs use an isolated per-job network with only their fixed proxy
helper as a peer. Host mounts contain this job's private work copy, output,
supervisor, per-worktree/per-image build caches, shared download cache and private
disk temporary directory; services add their own request/reply spool mounts.
The work copy is built from the manifest into `work.pending` and renamed into place:
small files are copied with modes 0644/0755, and each file over 512 MiB becomes a
symlink to `/sparkdev-source/<path>`. Only a manifest with large files adds one
read-only bind of its whole snapshot at `/sparkdev-source`; no project path text ever
appears in Docker arguments, the job cannot write or chmod a shared store inode, and a
small-only manifest needs no snapshot mount. A work copy that
cannot be built fails the job with exit 125 (`insufficient_disk` or `work_copy_failed`).
The Docker socket, host home, Ollama and PhaseForge services are absent. The shared
download cache is intentionally writable across Spark jobs; the internet switch
does not isolate those files or make mutually untrusted projects safe to share it.
CUDA uses the same limits plus an active `MemAvailable` watchdog; on GB10 this is
necessary because CUDA unified-memory allocations can evade the cgroup RAM limit.
The watchdog depends on the running controller and is not a hardware memory quota.
A bounded physical service test on 2026-10-21 allocated and filled a 16 GiB CUDA
tensor under an 8 GiB cgroup limit: PyTorch reported 16 GiB allocated/reserved,
but cgroup memory usage was about 391 MiB. The service was explicitly cancelled
and memory recovered. Docker's memory cap therefore did not enforce a hard GPU
allocation cap on this GB10. The test left ample host headroom and did not verify
a watchdog kill under memory pressure; see [named services](SERVERS.md).

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
restore them under their selected internet policy. Dependency preparation never
runs npm lifecycle scripts with egress; arbitrary normal jobs are not that fixed
preparation operation and may run their own scripts.
pip runs isolated, so a project file beside the lock (for example `json.py`) never
executes in the network-attached container, and every inherited `PIP_*` and `PYTHON*`
variable is replaced with fixed values. A site directory is built under a pending name
and replaces the previous one only after success. Jobs put prepared site directories
ahead of any `PYTHONPATH` the image already sets, in the sorted lock order preparation
uses. Because every prepared environment of a runtime shares that one `PYTHONPATH`, a
lock that pins a distribution at a different version than an earlier lock, or ships a
module an earlier lock's environment already provides from a different distribution or
version, is refused; identical copies and merging namespace packages are allowed.

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

### Public job internet

The 0.4.1 owner policy defaults to enabled when no `internet_policy` setting exists.
Normal jobs and services get their own internal Docker network, a Python-image
public proxy and a deny file covering connected IPv4 prefixes and the Spark's
local addresses. The proxy resolves a destination once, rejects the entire DNS
answer if any address is private/non-global or denied, then connects only to a
checked socket address. It refuses IPv6 destinations and verifies that a job's
Docker bridge cannot provide an IPv6 bypass. Home/private ranges, loopback,
link-local/cloud metadata and the Spark are blocked. Plain HTTP and HTTPS CONNECT
are supported; raw sockets, UDP and proxy-unaware network clients are not.

Public mode does not reuse the locked downloader's host allowlist or old transfer
size/lifetime caps. Fixed proxy environment variables expose that public path to
supported clients, with loopback excluded for local service calls. No CA override
is installed. A missing Python helper or a failed setup guard is reported and the
job runs offline; Docker unavailability never creates a duplicate job. Turning
internet off removes running helpers on the next scheduler tick. Server handling
also records `internet_off` when the running service must stop. Usage records are
seven integer counters, not URLs, headers or bodies. These controls restrict
destinations; they cannot stop a job from sending data to a public endpoint.

### Saved download storage

`shared/cache`, `shared/prepare` and never-mounted `shared/trash` are owner-checked
0700 directories under the bridge's own state. Runs mount `/cache` and a private
disk-backed `jobs/<id>/tmp` at `/var/tmp`. HF, torch, pip, uv and XDG cache defaults
point into `/cache`; arbitrary reusable files belong in `/cache/files`.
`HF_TOKEN_PATH` and `HF_MODULES_CACHE` remain under job-private `/tmp`.
Torch extensions, Triton, TorchInductor and CUDA code caches stay under private
`/var/tmp`. No `XDG_CONFIG_HOME` or `XDG_DATA_HOME` override is added. Preparations
mount only `/prepare-cache` for pip downloads, retaining per-project build caches,
locked installed environments and their own temporary space. This avoids letting
normal jobs poison a locked preparation's shared wheel download cache.

All jobs can modify saved downloads: this is a single owner trust domain, and an
offline job can still read or change it. Do not store secrets there. Known cache
roots are repaired before job creation through directory descriptors and no-follow
opens; substituted links/files are moved to trash without following them.

The `downloads` RPC allows reads by every client; `keep_gb` and `free_up` require
the `human` accounting label. Default keep is 200 GiB, with saved limits from
20 GiB up to disk size. Clearing is deferred rather than refused while busy.
Maintenance runs only from `serve()` under `scheduler.lock`, in resumable steps
with a 0.25-second work budget. It removes finished job temporary storage only
after the container is absent from Docker's running-name list. Shared-cache
clearing, measuring and eviction wait until no job is starting/running; clearing
and each eviction also require a successful empty `docker ps` job-name guard.
It deletes only quarantined trash, never follows symlinks, and verifies the inode
and every ancestor before moving a unit.

Usage measures allocated `st_blocks * 512`, deduplicating hardlinks, with a
2,000,000-entry scan ceiling. It ignores directory atime and clamps future times.
Measured units survive busy periods; a candidate is measured again if a job ran
after its measurement. When above the keep budget or below 128 GiB free, units
unused for 48 hours are selected oldest first until usage is at most 90% of the
budget and free space reaches the floor. Recent content can prevent either target.
The filesystem atime probe is not proof of Docker bind-mount atime behavior.
Status and devices read stored usage and the existing disk probe, never recursively
walk the cache. Their `downloads.v:1` object contains `folder`, `files_folder`,
`used_bytes`, `keep_bytes`, `free_bytes`, `floor_bytes`, `measured_at`, `over_limit`,
`clear_pending`, `atime`, `items` (at most 20) and `last_cleanup`.

### Named HTTP services

`serve` creates a service job, with a unique active name, rather than a public
listening endpoint. At most two services share the scheduler's total CPU/memory
budget. Each uses two CPUs and 1–48 GiB memory within the owner's total budget;
the default is 16 GiB. Service requests are exempt from batch per-job/client caps
and worktree claims, but not total admission or reserved memory headroom. Ready
services are not subtracted twice from the live `MemAvailable` check; their total
configured allocation still counts against the scheduler's aggregate limit.

The worker forwards spool requests through `http.client` to loopback inside the
same container. There is no published port, host network, Docker socket or direct
cross-job network path. Requests are mounted read-only; replies are a separate
writable mount. The controller distrusts container replies: it opens nonblocking
and no-follow, requires a regular file with one link, bounds its size, and computes
the body hash itself. FIFO, symlink and hardlink substitutions are refused.

Call IDs remain in SQLite for 24 hours so reconnect retries cannot repeat a call
after its files expire. At most 32 calls wait; input is at most 1 MiB and output
256 MiB, returned in chunks of at most 2 MiB. Files expire 10 minutes after delivery
or one hour after creation. Automatic wake is restricted to eligible recent idle,
lifetime or internet-off stops, never an explicit user stop or crash. Same-name
replacement across clients/projects requires `replace`. See [SERVERS.md](SERVERS.md)
for the CLI lifecycle, stop reasons, example and remaining physical acceptance.

## Evidence boundaries and operational limits

Python tests use an in-memory Docker stand-in for lifecycle fault injection; they
prove controller decisions, not container isolation by themselves. Real runtime build
and probe receipts live on the Spark; CLI acceptance receipts identify actual job
IDs and observed outcomes. Phone-hotspot roaming, physical laptop sleep, real Claude
Code invocation, Windows installation behavior and Marketplace admission require
their own evidence. None follows from unit tests or a client label. Large-file
transfer (5 GB and 16 GiB files) has unit-test coverage only so far; owner acceptance
receipts recording MB/s, link type and seconds per GiB are still required.

Logs are limited to 256 MiB per batch job; exceeding the bound kills the workload
with exit 125. A service writes one truncation marker and keeps draining output.
Normal runs/services may write files of at most 64 GiB (`--ulimit fsize`);
preparations retain their 4 GiB bound. Jobs can read synced inputs of up to 16 GiB.
Jobs and source snapshots are retained
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
