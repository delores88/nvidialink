# Agent and human CLI

`sparkdev` never prompts, invokes a pager, allocates a TTY, or interpolates workload arguments into SSH shell text. `--json` produces structured output. Attached runs stream JSON log events followed by the final result; detached commands return one JSON object. Client labels support accounting and fairness; they are not evidence that a particular AI agent actually ran a command.

## Setup and state

Run setup with your existing SSH alias and pinned `known_hosts` identity. All identifiers below are examples, not owner configuration.

```powershell
sparkdev setup --ssh-alias my-spark --host 192.0.2.10 --host-key-alias pinned-spark --identity-file C:/Users/You/.ssh/spark --user owner --local-build-gate C:/path/to/buildlock.py --json
sparkdev doctor --json
```

The key path is referenced, never read, copied, or uploaded by DGXBridge. OpenSSH performs authentication in BatchMode with strict known-host verification. Multiple `--host` values are tried on connection refusal, timeout, DNS failure, or unreachable network. Authentication and host-key errors fail closed and prevent remote fallback. `--allow-remote` explicitly permits the separately paired `sparkdev-link` transport after LAN reachability fails. Pair through the desktop QR workflow after SSH bootstrap. No owner-operated relay is assumed.

State defaults to Windows LocalAppData plus `SparkDevBridge`, and `~/.local/state/sparkdev` on Linux. `SPARKDEV_HOME` overrides the shared CLI/link state root. Configuration, pairing identity, receipts and logs stay outside the repository. Do not point state at a source tree. SSH bootstrap separately installs the controller under `~/sparkdev`.

Windows packaged applications can redirect AppData file I/O even when the known-folder API returns the normal owner's path. `connection` reports the configured and physically resolved state roots, package context and redirection. Setup refuses a detected redirected folder. Use the desktop's generated agent prompt/MCP snippet to select the verified owner state; do not copy keys or silently create a second configuration. No automatic config-only migration is performed.

## Connection and resource controls

```powershell
sparkdev --json connection
sparkdev --json connection --mode lan-only
sparkdev --json connection --mode remote-only
sparkdev --json connection --mode both
sparkdev --json connect
sparkdev --json disconnect
sparkdev --json resources
sparkdev --json resources --file C:/settings/spark-limits.json
sparkdev --json connection --laptop-jobs 2
```

LAN-only uses pinned SSH. Remote-only skips SSH and requires existing authenticated pairing; it does not guarantee iroh uses a relay. Both prefers LAN and falls back only after reachability failure. Authentication or host-key rejection never falls back. Connect enables the saved route and probes it; a failed probe leaves the configured choice enabled and returns a failure. Disconnect blocks new requests without revoking pairing or cancelling admitted jobs. Existing laptop workers retain their connection snapshot for lease heartbeats.

`resources` reads physical Spark bounds and effective policy. The JSON file is an object with `cpu_per_job`, `memory_gb_per_job`, `cpu_total`, `memory_gb_total`, `client_jobs`, `client_cpu` and `headroom_gb`. Saving validates relationships and hardware bounds, persists the controller policy, and returns the effective readback. Changes govern new requests/admissions; running jobs retain their original resources. After the first explicit policy save, project overrides are clamped to saved per-job caps. Default laptop concurrency is two and can be changed separately. Laptop native CPU and memory caps are not enforced by this release.


## Submit, poll and reattach

```powershell
sparkdev --client codex --json run C:/work/project --where spark --runtime rust --detach --request-id build-001 -- cargo test --locked
sparkdev --client codex --json wait JOB_ID --timeout 20
sparkdev --client codex --json logs JOB_ID --since 0
sparkdev --client codex --json logs JOB_ID --since NEXT_OFFSET
sparkdev --client codex --json status --all
sparkdev --client codex --json cancel JOB_ID
```

Use the returned `next_offset` for subsequent logs calls. Offsets count bytes, and `data` is base64 for lossless retrieval; `text` is a UTF-8 presentation. Every captured chunk is saved locally at its original byte offset. Log queries are repeatable. SSH requests have bounded connection and process timeouts; `wait` accepts 0â€“60 seconds of server waiting. Network setup adds bounded transport overhead. An attached run passes through the workload exit code; its JSON contains `process_exit_code`. Detached submission, wait and logs return success for a valid operation even if a completed workload failed: inspect `state` and `exit_code`.

Reuse the same `--request-id` after an ambiguous submission disconnect. The immutable request is saved before network submission and cannot be reused for different source, argv, runtime or placement. Without an explicit ID each new invocation is a new request. No-cache controls result reuse, not request identity. Run requests survive laptop disconnection once accepted by the Spark; cancel requests made offline are queued and delivered at the next authenticated command.

`--argv-json C:/path/args.json` accepts a JSON array of strings instead of arguments after `--`. It is useful when shell-specific quoting is difficult. Do not put credentials in argv or that file.

```json
["python3", "-c", "import sys; print(repr(sys.argv[1:]))", "argument with spaces", "", "$literal; & text"]
```

## Placement and shared board

`sparkdev devices --json` reports the laptop's observed OS, architecture and process-visible logical parallelism, local job receipts, and the authenticated Spark scheduler board, limits and configured runtimes. Unreachable or unconfigured Spark state carries an explicit error. Logical parallelism and scheduler limits do not indicate idle processors or free memory; those values remain unknown. Device inspection does not reserve capacity or deliver queued cancellations.

Spark `availability` is `reachable`, `unreachable` or `unconfigured`. Transport `route` is `lan_ssh`, `paired_iroh` or `none`, with `authenticated` and bounded `attempted_routes` explaining LAN reachability errors, authentication rejection, disabled/unconfigured remote access or adapter failure. The paired route does not claim that iroh used a relay or a direct connection; the adapter does not report that physical path. A completed strict SSH exchange followed by controller failure remains visible as authenticated transport with a controller error. Authentication rejection never triggers fallback.

Laptop process availability is separate from managed admission. Both runners need the shared Spark scheduler: when it is unreachable or unhealthy, `can_submit:false` and `admission.available:"unavailable"` disclose that dependency even though the laptop itself is available. When the scheduler is healthy, `can_submit:null` and `admission.available:"requires_request_check"` mean that runtime, quota, resource and worktree checks are still needed. There is no automatic offline laptop fallback or platform-incompatible rerouting.

`sparkdev plan DIR --where auto --runtime rust -- cargo test --locked` previews the same placement policy used by run. `--argv-json FILE` is also supported. Planning validates the source and exact arguments, computes the manifest without writing transfer objects, and returns `admission_required:true`, `claimed:false`, `uploaded:false` and `submitted:false`. It needs no network or SSH configuration. The result is a preview, not a reservation; run revalidates the tree and runtime against current scheduler admission. Runtime availability is not verified by plan.

`--where auto` chooses the laptop for explicit Windows/MSVC targets, PowerShell/cmd scripts, Windows sandbox commands, NSIS tools and Tauri installers. Other work goes to the Spark. Use `--where laptop` or `--where spark` when the command has requirements the classifier cannot infer. The selected machine and reason are included in submission output. Runtime defaults are `rust` for cargo/rustc/rustup, `node` for node/npm/npx and `python` otherwise; `--runtime` selects only an owner-provisioned registry name.

The laptop runner executes native owner processes and is not Docker-sandboxed. It obtains the same worktree lease from the Spark scheduler before starting, so it requires an authenticated scheduler connection. Laptop concurrency follows the saved owner setting (default two). Direct Cargo invocation is wrapped in the configured build gate, with per-project CARGO_TARGET_DIR under application state. On this owner's machine all local Cargo commands must use the prescribed PhaseForge build gate unchanged, and state/build output must stay on C:. Commands that indirectly invoke Cargo must themselves use the owner's build gate; the CLI cannot inspect arbitrary script behavior.

Laptop workers persist independently of the invoking CLI. On Windows they launch without inheriting the caller's capture pipes, run at lower process priority, send a shared-board heartbeat, and stop their owned process tree on cancellation or a lost lease. An expired laptop lease remains blocking on the scheduler because a sleeping laptop may resume a process. This protects the worktree from a second job; explicit terminal acknowledgement releases it. Final laptop log/board updates interrupted by a disconnect are saved and retried at the next authenticated command. `status --all` returns scheduler jobs plus laptop receipts and indicates unavailable remote state. Jobs for distinct worktrees may run independently on both machines. Laptop successful-result caching is disabled because the native host toolchain has no locked digest. Spark result reuse is scoped to the same worktree and requires matching source, exact argv, pinned runtime, worker and resource policy. Different worktrees keep isolated caches and do not share completed results. `--no-cache` forces a new execution.

## Sync and locked dependency preparation

```powershell
sparkdev sync C:/work/project --check --json
sparkdev sync C:/work/project --json
sparkdev prepare C:/work/project --runtime rust --json
```

Sync checks every included file before network upload. Git inventory includes tracked and untracked non-ignored files. Plain directories use a guarded walk. Build outputs, symlinks/reparse points, paths outside the root, credential/key names, secret directories and recognizable credential contents are refused or excluded. Source files named `secrets.rs` are permitted; actual credential-shaped content is not. This is a conservative safety filter, not a guarantee that arbitrary unknown secret formats can be detected. Transfer uses verified SHA-256 1 MiB chunks; retries upload only missing objects. Deleted files disappear in the next immutable snapshot.

Files are streamed: each is read once in 1 MiB chunks, hashed per chunk and whole, and scanned with 64 KiB of look-ahead across chunk boundaries, so memory use does not grow with file size. A file whose length changes while it is read is refused with exit 3 ("retry"). A project may hold files of up to 16 GiB and 32 GiB in total, of which files of 512 MiB or less may total 8 GiB; at most 100,000 files. `sync --check` reports `bytes`, `small_bytes`, `large_files` and these `limits` without writing transfer objects. Files of 512 MiB or less are staged in the local transfer store only while at least 4 GiB of laptop disk stays free; larger files, and any chunk missing from the store, are read again from the source at their offset and verified against the manifest hash ("Source changed during sync; rerun" if they differ).

A sync first asks the Spark which limits it advertises. A controller that predates this release advertises none, and then any file over 512 MiB or project over 8 GiB is refused before upload with "update Spark setup"; redeploy the controller through the desktop setup. A current controller is asked to commit first, so an unchanged project needs two requests. Missing chunks are uploaded in first-occurrence order, in batches of at most 8 MiB over LAN SSH and 3 MiB over the paired route, halved after a transport timeout, in at most three rounds. A snapshot with more than 512 MiB of new bytes is assembled in the Spark's background: the CLI polls, and exits 6 asking you to rerun if the reported progress stalls for 10 minutes. Rerunning resumes; nothing already uploaded or assembled is repeated. Ambiguous transport failures (exit 4) are retried up to five times (2, 4, 8, 16, 30 s) once the sync has reached the Spark, after re-reading the saved connection state; a disconnect by the owner, LAN-only refusal, authentication failure (7) or the commit request to an older controller is never retried. The paired route carries at most a 4 MiB request, so a manifest of very many files may need LAN SSH. The Spark keeps 64 GiB of its disk free: below that, syncs and job admission wait and report the free and reserve figures (`status.storage`, `devices` Spark `storage`). Sync output adds `spark_limits`, `spark_storage`, `sync_rounds`, `materialize_polls` and `transfer_retries`.

`prepare` submits a fixed runtime-specific locked downloader, with the same job/wait/logs interface. It cannot run caller-supplied setup commands. Normal jobs remain network-disabled. Dependency egress uses the separately installed allowlisting proxy and fails closed if it is unavailable. `--runtime cuda` installs `requirements-cuda.lock` in the CUDA image without a GPU and refuses packages the image provides; `status --json` lists the Spark's `preparable_runtimes`. See the remote controller docs for registry provisioning and supported lockfiles.

## MCP

Start `sparkdev --client codex mcp` or `sparkdev --client claude mcp` as an MCP stdio server. It supports initialization, tools/list, and devices/plan/prepare/run/wait/logs/status/cancel through the same core. Inspect devices and plan placement before submission; neither reserves capacity. Run and locked dependency preparation detach; wait is bounded. Server launch arguments bind the accounting client, and tool calls cannot override it. JSON-RPC output contains no progress chatter. The transport uses newline-delimited JSON with a 2 MiB message limit. Real agent-session acceptance is separate from these protocol tests.

## Stable control exit codes

| Code | Meaning |
| --- | --- |
| 0 | Command succeeded; inspect job state for detached work |
| 2 | Invalid arguments, JSON or immutable request conflict |
| 3 | Local configuration, filesystem or process problem |
| 4 | Connection failure or bounded transport timeout |
| 5 | Source/credential/path policy rejection |
| 6 | Remote operation or protocol integrity failure, including a stalled Spark snapshot (rerun to continue) |
| 7 | SSH authentication or pinned host-key failure; no fallback |
| 8 | Laptop admission or worktree claim conflict |

Attached workload exit codes take precedence and may overlap control codes. Distinguish them through `process_exit_code` versus `{ok:false,code,message}`.

## Copy-paste agent instructions

For AGENTS.md or CLAUDE.md in a project that uses this utility:

> Prefer sparkdev for Linux-capable builds, tests and heavy compute. Use your actual client label (`codex` or `claude`), `--json`, `run --detach` with a retained request ID, bounded `wait --timeout 20`, and incremental `logs --since OFFSET`. Check `status --all` before submitting. Keep Windows installers and Windows-only tests local using `--where laptop`. Never claim another agent's label as acceptance evidence. Treat host-key rejection as a hard stop; never disable host verification. After tool interruption reuse the request ID or job ID, rather than submit duplicate work.

Codex requires process-launch permission for the installed executable and SSH/paired-link network access. In restricted environments allow those capabilities explicitly through the environment's existing controls; never weaken SSH identity verification. The owner's present session is unrestricted, which does not prove behavior in a different Codex sandbox.
