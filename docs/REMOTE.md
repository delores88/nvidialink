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
Docker's official NGC image retains its own licenses; Spark Dev Bridge does not
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
| `missing` | `hashes` (at most 10,000) | `missing` hashes |
| `put` | `sha256,data` (base64, at most 1 MiB decoded) | accepted hash and size |
| `put_many` | `chunks:[{sha256,data}]` (128 objects / 3 MiB JSON body maximum) | ordered `accepted` objects and decoded byte count |
| `sync` | `project,manifest` | `manifest_id,files,bytes,project` |
| `run` | `project,manifest_id,argv,runtime,client,request_id,no_cache?` | job receipt |
| `prepare` | `project,manifest_id,runtime,client,request_id` | dependency job receipt |
| `wait` | `job,timeout?` (0–60 seconds) | current job receipt |
| `logs` | `job,since,limit?` (1 MiB maximum) | base64 `data`, display `text`, `next_offset`, receipt |
| `cancel` | `job` | durable cancellation receipt |
| `status` | `all?` | shared laptop/Spark board, scheduler heartbeat, limits, runtimes |
| `doctor` | none | status plus SQLite, Docker, Linux and memory checks |
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
Maximum file/tree sizes are 4/20 GiB and maximum file count is 100,000.

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

## Isolation, fairness and dependency access

Default jobs use 4 CPUs and 8 GiB, with at most 16 CPUs / 64 GiB admitted total,
two jobs / eight CPUs per client, and at least 32 GiB reserved memory headroom.
Admission conservatively includes all reserved job memory. Preparation proxy overhead
also counts against quotas. The scheduler rotates FIFO admission between `claude`,
`codex`, and `human`; these labels are attribution, not proof which agent ran a job.
Each worktree has one active execution claim. Stale laptop claims remain blocking
until the client stops its process and releases/reports completion: sleep cannot
prove that a workload stopped exactly when a lease expired.

An owner-local, protected `~/sparkdev/limits.json` can grant a named worktree up to
8 CPUs / 32 GiB without changing defaults or global/client quotas. Its schema is
`{"v":1,"projects":{"PROJECT_SHA256":{"cpu":8,"memory_gb":32}}}`. Callers cannot
change it through RPC. The admission record, cache key, Docker limits and thread
environment all use the same resolved grant. Dependency preparation keeps its
conservative default limits. Owner configuration must never be committed.

The container is nonroot, read-only-root, cap-drop ALL, no-new-privileges, PID limited,
CPU/memory/swap limited, nice 10, bounded tmpfs, and network-none. Host mounts contain
only this job's private work copy, output, supervisor and per-worktree/per-image caches.
The Docker socket, host home, Ollama, PhaseForge services and other caches are absent.
CUDA uses the same limits plus an active `MemAvailable` watchdog; on GB10 this is
necessary because CUDA unified-memory allocations can evade the cgroup RAM limit.
The watchdog depends on the running controller and is not a hardware memory quota.

Dependency preparation is a separate fixed job. Rust runs `cargo fetch --locked`,
Node runs `npm ci --ignore-scripts`, and Python requires `requirements.lock` and
`pip --require-hashes --only-binary=:all:`. Lockfile hashes must remain unchanged.
Npm installed trees and Python site packages are saved by lock hash; actual jobs
restore them while remaining offline. Npm lifecycle scripts are never given egress.

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
their own evidence. None follows from unit tests or a client label.

Logs are limited to 256 MiB per job; exceeding the bound kills the workload with
exit 125. Individual files are capped at 4 GiB. Jobs and source snapshots are retained
for inspection; no garbage collector currently deletes them automatically. The
controller refuses new jobs at 4096 retained records. Owners should monitor disk
space and archive old completed work deliberately. There is no general remote output
artifact download command yet; CLI logs and exit-code receipts are implemented.
