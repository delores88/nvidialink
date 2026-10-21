# Native tools for Claude Code and Codex

The installed `sparkdev` CLI is the primary interface. Its `mcp` command exposes connection, resources, devices, plan, prepare, run, wait, logs, status and cancel through the same core over stdio. The 0.4.0 candidate adds `serve` and `call` for named Spark HTTP services. No TCP listener, token copying or separate model account is required by the MCP server. Each AI product still uses its own existing authentication. Candidate features need the matching CLI and Spark controller; the older real-session evidence below does not verify them.

Use the executable and state paths shown by the desktop **Agent setup** drawer screen. Its copyable prompts and snippets select the verified owner state and explain package redirection. The examples below omit an environment override; add the generated `SPARKDEV_HOME` environment entry when shown. On Windows the NSIS install bundles it in the application's `bin` directory. Replace the example below with that exact installed path; do not point production configuration at a development build.

Claude Code project `.mcp.json`:

```json
{
  "mcpServers": {
    "sparkdev": {
      "type": "stdio",
      "command": "C:/Users/You/AppData/Local/DGXBridge/bin/sparkdev.exe",
      "args": ["--client", "claude", "mcp"]
    }
  }
}
```

Codex `~/.codex/config.toml`:

```toml
[mcp_servers.sparkdev]
command = "C:/Users/You/AppData/Local/DGXBridge/bin/sparkdev.exe"
args = ["--client", "codex", "mcp"]
tool_timeout_sec = 90
```

Merge these entries with existing configuration; do not replace unrelated servers or settings. Restart/reconnect the agent's MCP connection after changing configuration. In Claude Code, review the project MCP server through the normal trust flow. In Codex, `codex mcp list` reports configured servers; tool availability in the actual session is the stronger check.

Start with `devices` to observe whether Spark is reachable, the authenticated route selected, attempted routes and failure reasons, and whether the shared scheduler can be reached. LAN SSH and paired iroh are distinguished. A paired route does not by itself prove traffic is using a relay; unobserved direct/relay details remain unknown. Laptop hardware availability is separate from job admission: the current laptop runner also needs the shared scheduler on Spark. An unavailable scheduler means no new managed laptop jobs, even when the laptop itself is available. Neither this observation nor reported scheduler limits reserve capacity or establish free CPU/memory.

`devices` also reports the Spark internet policy and latest saved-download measurement when the controller supports them. Spark jobs save downloads in `/cache`, shared by every job; keep cache defaults and put large reusable files in `/cache/files`. `--no-cache` bypasses completed-result reuse, not these downloads. Only the owner changes or clears saved downloads; no cache-clearing tool is exposed in MCP. Public internet is on by default in 0.4.0 through a per-job proxy that blocks home/private networks, this Spark and cloud metadata; raw sockets are not supported. This policy does not isolate the shared cache. Ask the owner before changing the internet/resource policy.

Use `plan` with `dir`, an exact `argv` array and optional `where` and `runtime` to preview placement and validate source files locally. It does not upload files, claim the worktree or submit a job. Runtime availability and request-specific admission are checked on submission. Arbitrary scripts can hide platform requirements; use explicit placement when needed.

The `run` tool takes the same planning arguments plus optional `request_id` and `no_cache`. It returns after source synchronization and job admission rather than waiting for the workload. Initial synchronization can exceed a short tool timeout on large projects: uploading gigabytes takes minutes, and a snapshot with more than 512 MiB of new bytes is assembled in the Spark's background while the CLI polls. Pre-sync such projects from a terminal with `sparkdev --json sync DIR` (rerun it after an interruption; it resumes), then call `run`, which finds the snapshot already committed and takes two quick requests. Retry uncertain admissions with the same request ID. Projects may hold files up to 16 GiB and 32 GiB in total, at most 8 GiB of it in files of 512 MiB or less; against a Spark controller that predates these limits, files over 512 MiB are refused until Spark setup is updated. Retain the resulting job ID. Call `wait` with `timeout` from 0 to 60, then `logs` with `since` set to the last `next_offset`. A successfully handled tool call can describe a failed workload; inspect its `state` and `exit_code`. The server binds the accounting client from its launch arguments; callers cannot change it in tool arguments.

An agent can submit Linux-capable work to the Spark and Windows-specific work to the laptop. Agents see the same board. A client label alone does not authenticate a particular model or prove a Claude/Codex session made the call. CodaClauda, a separate product, owns agent-to-agent conversations and delegation; this server owns compute jobs only.

Find my Spark (`sparkdev discover`) is an owner-only desktop action. It is not an MCP tool, no tool argument reaches it, and agents must not run it. Its `--client human` check is advisory, like every client label: it prevents accidents, not a misbehaving agent with a shell.

`prepare`, `status`, remote `wait` and remote `logs` also deliver cancellations previously queued by the owner when they reconnect. Their tool annotations disclose this mutating effect; they do not request new cancellations. `logs` saves captured output locally. `devices` and `plan` are the observation tools and do not deliver queued cancellations.

For first-time dependency preparation call `prepare` with `dir`, `runtime` (`rust`, `node`, `python` or `cuda`) and an optional stable `request_id`, retain its job ID and poll it to success. Preparation accepts no arbitrary commands or laptop placement. Preparation uses fixed lockfile commands through the domain allowlist and a separate prepare-only pip cache, independent of normal jobs' internet setting. CUDA preparation installs `requirements-cuda.lock`, a hash-locked no-deps closure, without a GPU; packages the CUDA image provides are refused. `status` reports the Spark's `preparable_runtimes`. The equivalent CLI is `prepare DIR --runtime rust|node|python|cuda --json`. See [CLI.md](CLI.md) for setup and errors.

For a reusable loaded model, `serve` takes source, an exact argv array, a name and bounded memory/idle settings, then returns a durable job ID. `call` sends one HTTP request to that name without opening a public port. Retain its request ID, retry the same payload with that ID after an uncertain result, and poll a `waiting` result rather than create duplicate work. The MCP call loop is bounded to 45 seconds. `logs`, `wait` and `cancel` accept service names; name cancellation requires a live connection. See [SERVERS.md](SERVERS.md) for payload limits, wake behavior and the distinction between a completed call and a successful 2xx application response.

Codex process-launch and network permissions still apply. An MCP connection does not weaken SSH host-key checking, bypass an OS sandbox or give arbitrary processes permission to run. This repository's live unrestricted environment proves only that environment; a restricted Codex installation needs its own permitted-process/network test.

The [real-session receipt](../evidence/real-agent-mcp.json) separately records **MCP acceptance passed** and **Codex Windows sandbox acceptance failed**. Fresh Claude Code and Codex sessions each called `devices`, `plan`, `run`, `wait` and `logs` through the installed CLI. Their two jobs were observed running together on the physical Spark and both completed with exit code 0 and complete fixture markers. The tested CLI SHA-256 is `21bfcc929b6b82be859b5c0e22e523cd14ac86255fc26bec27c6ae90012c31e7`. This proves the LAN MCP workflow; it does not establish hotspot, laptop sleep, or the separate restricted shell workflow.

For that authorized fixture run, the harness exposed only `devices`, `plan`, `run`, `wait` and `logs` to Codex and supplied invocation-only `approval_mode="approve"` overrides for `run`, `wait` and `logs`. No saved server trust or global configuration changed. These [per-tool approval settings](https://learn.chatgpt.com/docs/extend/mcp?surface=cli) are independent of OS sandbox permissions. The separate `workspace-write` doctor command failed with `workspace-write sandbox has no writable root capability SIDs`; the overall harness result therefore remains failed. The [first attempt](../evidence/real-agent-mcp-first-attempt.json), before scoped MCP approvals were supplied, is retained rather than replaced.

Sources checked 2026-10-13: [Codex MCP configuration](https://learn.chatgpt.com/docs/extend/mcp?surface=cli), [Claude Code MCP configuration](https://code.claude.com/docs/en/mcp), [MCP tools protocol](https://modelcontextprotocol.io/specification/2025-06-18/server/tools). Actual real-session receipts are recorded separately under evidence; these instructions alone are not acceptance.
