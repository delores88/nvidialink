# Native tools for Claude Code and Codex

The installed `sparkdev` CLI is the primary interface. Its `mcp` command exposes devices, plan, prepare, run, wait, logs, status and cancel through the same core over stdio. No TCP listener, token copying or separate model account is required by this server. Each AI product still uses its own existing authentication.

Use the executable path shown by the desktop **Agent setup** screen. On Windows the NSIS install bundles it in the application's `bin` directory. Replace the example below with that exact installed path; do not point production configuration at a development build.

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

Use `plan` with `dir`, an exact `argv` array and optional `where` and `runtime` to preview placement and validate source files locally. It does not upload files, claim the worktree or submit a job. Runtime availability and request-specific admission are checked on submission. Arbitrary scripts can hide platform requirements; use explicit placement when needed.

The `run` tool takes the same planning arguments plus optional `request_id` and `no_cache`. It returns after source synchronization and job admission rather than waiting for the workload. Initial synchronization can exceed a short tool timeout on large projects; pre-sync those projects using the CLI and retry uncertain admissions with the same request ID. Retain the resulting job ID. Call `wait` with `timeout` from 0 to 60, then `logs` with `since` set to the last `next_offset`. A successfully handled tool call can describe a failed workload; inspect its `state` and `exit_code`. The server binds the accounting client from its launch arguments; callers cannot change it in tool arguments.

An agent can submit Linux-capable work to the Spark and Windows-specific work to the laptop. Agents see the same board. A client label alone does not authenticate a particular model or prove a Claude/Codex session made the call. CodaClauda, a separate product, owns agent-to-agent conversations and delegation; this server owns compute jobs only.

`prepare`, `status`, remote `wait` and remote `logs` also deliver cancellations previously queued by the owner when they reconnect. Their tool annotations disclose this mutating effect; they do not request new cancellations. `logs` saves captured output locally. `devices` and `plan` are the observation tools and do not deliver queued cancellations.

For first-time dependency preparation call `prepare` with `dir`, `runtime` (`rust`, `node` or `python`) and an optional stable `request_id`, retain its job ID and poll it to success. Preparation accepts no arbitrary commands or laptop placement. Jobs execute without network. Preparation uses fixed lockfile commands through the domain allowlist. The equivalent CLI is `prepare DIR --runtime rust|node|python --json`. See [CLI.md](CLI.md) for setup and errors.

Codex process-launch and network permissions still apply. An MCP connection does not weaken SSH host-key checking, bypass an OS sandbox or give arbitrary processes permission to run. This repository's live unrestricted environment proves only that environment; a restricted Codex installation needs its own permitted-process/network test.

Sources checked 2026-10-13: [Codex MCP configuration](https://learn.chatgpt.com/docs/extend/mcp?surface=cli), [Claude Code MCP configuration](https://code.claude.com/docs/en/mcp), [MCP tools protocol](https://modelcontextprotocol.io/specification/2025-06-18/server/tools). Actual real-session receipts are recorded separately under evidence; these instructions alone are not acceptance.
