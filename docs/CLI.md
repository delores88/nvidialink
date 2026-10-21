# Agent and human CLI

`sparkdev` never prompts, invokes a pager, allocates a TTY, or interpolates workload arguments into SSH shell text. `--json` produces structured output. Attached runs stream JSON log events followed by the final result; detached commands return one JSON object. Client labels support accounting and fairness; they are not evidence that a particular AI agent actually ran a command.

## Setup and state

Run setup with your existing SSH alias and pinned `known_hosts` identity. All identifiers below are examples, not owner configuration.

```powershell
sparkdev setup --ssh-alias my-spark --host 192.0.2.10 --host-key-alias pinned-spark --identity-file C:/Users/You/.ssh/spark --user owner --local-build-gate C:/path/to/buildlock.py --json
sparkdev doctor --json
```

The key path is referenced, never read, copied, or uploaded by DGXBridge. OpenSSH performs authentication in BatchMode with strict known-host verification.

LAN connection order: within one command, an address that already authenticated is reused alone. Otherwise DGXBridge tries up to four remembered verified addresses (newest first), then the `--host` values in order, then the SSH alias's own `HostName`. That `HostName` is read locally with `ssh -G`, which opens no connection. Remembered addresses only reorder these current addresses: an address that is no longer a `--host` value or the alias's `HostName` is never dialed, so a re-run of setup takes effect at once. Each address is tried once. Every `--host` value is planned; the alias's `HostName` is added only while fewer than eight addresses are planned. Without `--host` values only the alias is dialed, through OpenSSH, as before. With them the bare alias is never dialed, only its concrete `HostName`.

Before dialing, when more than one address is planned, every concrete address gets a parallel TCP check of the SSH port (1.5 s per connect, 2 s overall). A single planned address is not checked: nothing could be dialed ahead of it, so it gets exactly the dial it had before rediscovery. An address that refuses, stays silent for the whole 1.5 s, or whose name resolves to nothing is reported as `host_unreachable` and held back. A held-back `--host` address keeps its place ahead of later addresses: it is dialed once more before a later address's failure that ran nothing remotely (a rejection or another failure before authentication) can end the command, and before giving up. That dial uses a 3 s connect timeout, what the 2 s check left of the 5 s each `--host` address had before rediscovery, so the check plus that dial never cost a dead address more than its one dial did, and a slow host the check missed still gets its own dial. A held-back alias `HostName` that is not a `--host` value is not dialed. When nothing answers, a command therefore costs no more than the serial dials before rediscovery, plus the local `ssh -G` lookup. A check that runs out of time, such as a slow name lookup, decides nothing: that address is dialed in order with OpenSSH's own timeouts. A command returns as soon as it has its answer and never waits for slower checks. When the alias uses ProxyJump, ProxyCommand, BindAddress or BindInterface, or `ssh -G` fails, nothing is checked locally, because ssh reaches every address through that route. An answering port is only a candidate: it must still pass the same pinned host-key check and key authentication.

Every dial runs OpenSSH with `LogLevel=VERBOSE`, which writes its "Authenticated to" line to stderr once key authentication succeeds and before the remote command starts. That stderr is read only to classify the result and is never shown or stored. Only OpenSSH's own failure (exit 255) can mean an unreachable address or a dropped connection; a remote command's own exit status never does, whatever it printed. An ssh exit 255 with no output and without that line ended before authentication; anything else may have run the command. Before authentication, connection refusal, timeout, DNS failure or an unreachable network moves to the next address.

A dropped connection counts as dropped during the handshake, with nothing run remotely, only when OpenSSH tags it with the identification exchange (`kex_exchange_identification`, `ssh_exchange_identification` or `banner exchange`) and neither the "Authenticated to" line nor any output came first. Every other drop, including an untagged "Connection reset by" or "Connection closed by", which OpenSSH prints the same way in every phase, may have run the command. A command repeats at most one dropped dial, at the same address with the identical pinned command, after a pause of 1 to 1.5 s: a handshake drop for any request, and any other drop only for requests the Spark controller treats as the same request when repeated (`status`, `logs`, `wait`, `doctor`, `cancel`, the sync and upload steps, laptop lease heartbeats and releases, a `resources` read, and run, prepare and claim requests with a request ID). A raw desktop command, a resource policy write or an unknown request is repeated only after a handshake drop. A deadline set when the request starts bounds this: after any drop each later dial must end by it, and neither the repeat nor another address is dialed without enough time left (10 s, plus the server-side wait a `wait` request asks for). Authentication failures, timeouts and process errors are never repeated. A drop that may have run the command ends the command at that address with exit 4 and no fallback, and the address stays remembered; the message says that the request may have run and whether it is safe to repeat, and for run and prepare it names the request ID to reuse. A handshake drop moves to the next address and keeps a remembered address; when nothing else serves the command, it fails with exit 4 and, like unreachability, allows remote fallback.

At a `--host` address, or at the alias when no `--host` is set, an authentication or host-key error ends the command with exit 7 and prevents remote fallback, and any other failure before authentication, such as an incompatible SSH server, ends it with exit 4, as before rediscovery; held-back `--host` addresses ahead of it get their dial first, and later addresses are not tried. A remembered address is only a learned hint. If it rejects the pinned host key or the login, or answers with an incompatible SSH server, it is forgotten and the next address is tried through the same pin; a dropped connection or a timeout is no evidence that it is stale and never forgets it. When that address is a `--host` value, a rejection still prevents remote fallback: the command fails with exit 7 unless another address authenticates. The alias's own `HostName` was not dialed before rediscovery when `--host` values are set, so any failure there that ran nothing (a rejection, an incompatible server, a handshake drop, too many authentication failures, or a transport timeout whose stderr shows it never authenticated) is reported in `attempted_routes` and the next address is tried; it never turns an otherwise unreachable result into exit 7. For every address, a failure after authentication, a remote command's own failure, a drop that may have run the command, or any other transport timeout stops at once without fallback. Addresses that authenticate are remembered in `lan-route.json` in the state directory: at most eight, four per SSH alias, pin, port and user. Each expires 30 days after it last authenticated; an address in daily use is re-verified in the file at most once a day. Route memory is not written when the state folder is redirected or its redirection cannot be determined. Automatic rediscovery never rewrites `config.json` and never scans the network; only the owner's Find my Spark action below searches, and only when asked.

## Find my Spark (owner only)

```powershell
sparkdev --client human --json discover
sparkdev --client human --json discover --save ADDRESS
```

The desktop's **Find my Spark** button runs these for the owner. They run only with exactly one explicit `--client human` (or `--client=human`) before any `--`; a bare call, `--client claude`, `--client codex` or a repeated `--client` is refused with exit 2. They are not MCP tools, and nothing runs them automatically: not `devices`, `status` or `connect`, not the desktop refresh, and not an unreachable-LAN error. Disconnected and Remote-only refuse them with exit 4, as they refuse SSH.

`discover` searches only this laptop's own networks, and only on Windows. It uses Ethernet and Wi-Fi hardware adapters that are up, are not tunnels or filters, and have a connector, a default gateway and a preferred RFC 1918 IPv4 address; a Hyper-V virtual switch or a VPN adapter is skipped. Each address's network is clamped to its /24 (a /25 to /30 stays as it is, a /31 or /32 is skipped), at most two networks are searched, and a wired and a Wi-Fi adapter on the same network are merged. An address is kept only when Windows routes it out of one of that network's own adapters, so a VPN that claims private ranges never carries the search. Each kept address gets one TCP connect to the configured SSH port (3 s each, 4 s overall). At most 32 answering addresses (configured addresses first, then the alias's `HostName`, then ascending) are checked, 16 at a time, with the same pinned `ssh` command every dial uses (BatchMode, `StrictHostKeyChecking=yes`, `HostKeyAlias`, never `accept-new` or a known_hosts write) running `true`, 8 s each. The alias must reach the Spark directly: ProxyJump, ProxyCommand, BindAddress or BindInterface refuses the search with exit 4, as does a failed `ssh -G`. The whole search takes up to about 25 seconds.

The result lists only the addresses that passed the pinned host key and login (`found`, each with the laptop adapter that reached it), the status of every configured address and of the alias's `HostName`, and counts: `open`, `unchecked` (could not be checked in time), `not_checked` (answered beyond the 32), `login_rejected`, `local_key_error`, `other_ssh_hosts`, `unknown_key_hosts` and `unverified`. Other devices appear only as counts. A changed host key counts as another device only when the same search proved the pin, through an address that authenticated or whose server refused the login after the host-key check; otherwise it counts as an unknown key, and `pin_hint` suggests checking the pinned known_hosts entry. A local key or SSH configuration error never proves the pin. The adapter and medium describe this laptop's side only, not the Spark's own link. `discover` writes nothing.

`discover --save ADDRESS` takes a private IPv4 address on one of those networks and routed through it, checks it again together with up to 15 configured addresses on the same networks, and saves nothing unless it authenticates: a host-key, login or local key failure is exit 7, and no answer is exit 4. It then makes the canonical address the first `--host` under the configuration lock, keeps every other address except configured ones this check proved to be another device, and names those in `removed`; an address that is closed, unchecked or answers with an unknown key is kept. When no `--host` was set and the alias has no `HostName` of its own, the alias stays as the second address. If the alias, pin, port, user, key file, connection or mode changed during the check, it writes nothing and asks for a new search (exit 4). Like setup it refuses a redirected state folder, or one whose redirection cannot be determined (exit 3). The saved address is also remembered in `lan-route.json`, and more than four saved addresses add a `warning`.

`--allow-remote` explicitly permits the separately paired `sparkdev-link` transport after LAN reachability fails. Pair through the desktop QR workflow after SSH bootstrap. No owner-operated relay is assumed.

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

Reuse the same `--request-id` after an ambiguous submission disconnect. The immutable request is saved before network submission and cannot be reused for different source, argv, runtime or placement. Without an explicit ID each new invocation is a new request. A connection failure (exit 4) of a Spark run or of prepare ends its message with the request ID, including a generated one, so it can be reused. No-cache controls result reuse, not request identity. Run requests survive laptop disconnection once accepted by the Spark; cancel requests made offline are queued and delivered at the next authenticated command.

`--argv-json C:/path/args.json` accepts a JSON array of strings instead of arguments after `--`. It is useful when shell-specific quoting is difficult. Do not put credentials in argv or that file.

```json
["python3", "-c", "import sys; print(repr(sys.argv[1:]))", "argument with spaces", "", "$literal; & text"]
```

## Placement and shared board

`sparkdev devices --json` reports the laptop's observed OS, architecture and process-visible logical parallelism, local job receipts, and the authenticated Spark scheduler board, limits and configured runtimes. Unreachable or unconfigured Spark state carries an explicit error. Logical parallelism and scheduler limits do not indicate idle processors or free memory; those values remain unknown. Device inspection does not reserve capacity or deliver queued cancellations.

Spark `availability` is `reachable`, `unreachable` or `unconfigured`. Transport `route` is `lan_ssh`, `paired_iroh` or `none`, with `authenticated` and bounded `attempted_routes` explaining LAN reachability errors, authentication rejection, disabled/unconfigured remote access or adapter failure. Each LAN attempt names its `source` (`sticky`, `remembered`, `configured` or `alias`) and the dialed `host`, which is null when OpenSSH resolved the alias. Its `status` is `authenticated`, `host_unreachable`, `authentication_failed`, `connection_dropped`, `ssh_command_failed` (an incompatible SSH server, or a failure with no evidence of anything else), `transport_timeout` or `process_error`. There is one record per dial, plus one for each address the precheck held back. A `connection_dropped` record adds `phase` (`handshake`: before authentication, nothing ran; `session`: the command may have run), `try` (1, or 2 for the repeat) and `retry` (`scheduled`, `exhausted`, `not_safe_to_repeat` or `insufficient_time`); any other record of a repeated dial carries `try: 2`. A Spark `error` from a dropped connection has `kind` `connection_dropped`. `target_index` is the position of the dialed address among the configured `--host` values, whatever its source, and null when the address is not one of them. An authenticated LAN route reports the dialed address as `lan_host`. The paired route does not claim that iroh used a relay or a direct connection; the adapter does not report that physical path. A completed strict SSH exchange followed by controller failure remains visible as authenticated transport with a controller error. Authentication rejection never triggers fallback.

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

`prepare` submits a fixed runtime-specific locked downloader, with the same job/wait/logs interface. It cannot run caller-supplied setup commands. Normal jobs remain network-disabled. Dependency egress uses the separately installed allowlisting proxy and fails closed if it is unavailable. See the remote controller docs for registry provisioning and supported lockfiles.

## MCP

Start `sparkdev --client codex mcp` or `sparkdev --client claude mcp` as an MCP stdio server. It supports initialization, tools/list, and devices/plan/prepare/run/wait/logs/status/cancel through the same core. Inspect devices and plan placement before submission; neither reserves capacity. Run and locked dependency preparation detach; wait is bounded. Server launch arguments bind the accounting client, and tool calls cannot override it. JSON-RPC output contains no progress chatter. The transport uses newline-delimited JSON with a 2 MiB message limit. Real agent-session acceptance is separate from these protocol tests.

## Stable control exit codes

| Code | Meaning |
| --- | --- |
| 0 | Command succeeded; inspect job state for detached work |
| 2 | Invalid arguments, JSON or immutable request conflict |
| 3 | Local configuration, filesystem or process problem |
| 4 | Connection failure, dropped SSH connection or bounded transport timeout; the message says whether the request may have run remotely |
| 5 | Source/credential/path policy rejection |
| 6 | Remote operation or protocol integrity failure |
| 7 | SSH authentication or pinned host-key failure; no fallback |
| 8 | Laptop admission or worktree claim conflict |

Attached workload exit codes take precedence and may overlap control codes. Distinguish them through `process_exit_code` versus `{ok:false,code,message}`.

## Copy-paste agent instructions

For AGENTS.md or CLAUDE.md in a project that uses this utility:

> Prefer sparkdev for Linux-capable builds, tests and heavy compute. Use your actual client label (`codex` or `claude`), `--json`, `run --detach` with a retained request ID, bounded `wait --timeout 20`, and incremental `logs --since OFFSET`. Check `status --all` before submitting. Keep Windows installers and Windows-only tests local using `--where laptop`. Never claim another agent's label as acceptance evidence. Never run sparkdev discover; Find my Spark is an owner-only action. Treat host-key rejection as a hard stop; never disable host verification. After tool interruption reuse the request ID or job ID, rather than submit duplicate work.

Codex requires process-launch permission for the installed executable and SSH/paired-link network access. In restricted environments allow those capabilities explicitly through the environment's existing controls; never weaken SSH identity verification. The owner's present session is unrestricted, which does not prove behavior in a different Codex sandbox.
