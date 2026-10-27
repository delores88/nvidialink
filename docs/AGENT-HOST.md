# Agent host

Status: design for 0.7.0, branch `claude/070-agent-host`.

Coding agents (Claude Code and Codex) do their tool work on the laptop: git, cargo, rustc, node and test runs. With many agents the laptop is the bottleneck while the Spark sits idle. The agent host runs a whole agent session on the Spark (or another added Linux or WSL computer) and brings its work back as a git ref. The laptop only submits and receives.

## What the owner sees

- Each device that can host agents shows **Agents** with one row per tool: *Signed in*, or a **Sign in** button.
- Sign-in happens once per tool, per device:
  - **Claude Code:** the owner runs `claude setup-token` on the laptop and pastes the token into RTXBridge.
  - **Codex:** RTXBridge starts Codex's device sign-in on the Spark and shows the code and link; the owner approves it in the browser.
- Agent jobs appear on the job board and the map like any other job, as their own kind.
- When one finishes, its commits are already in the laptop repo as `rtx/agent/<job>`. There is no fetch step.

## Interface

```text
sparkdev agent run REPO --tool claude|codex --prompt-file FILE [--branch B] [--device spark|ID] --detach --request-id R
sparkdev agent login claude --token-stdin        # owner only; token on stdin
sparkdev agent login codex                       # owner only; prints the device code
sparkdev agent status                            # sign-in state per tool and device
```

`wait`, `logs`, `cancel` and `status` are the existing job verbs. MCP gets `agent_run` with the same fields.

## How it works

1. **In.** The laptop writes `git bundle create` of the chosen branch, with its history up to the merge base the Spark already has. It adds the prompt and a small request record to a staging folder. That folder goes through the existing project sync, so it works over the LAN and over the paired remote route alike. No git server, no second SSH path.
2. **Run.** The controller runs a normal job with runtime `agent`:
   - It uses the existing sandbox: non-root, read-only root, `--cap-drop ALL`, `no-new-privileges`, CPU, memory and pid limits, the headroom watchdog and scratch on tmpfs.
   - A fixed driver script, copied into the job record like `worker.py`, does the rest:
     - clones the bundle and creates branch `agent/<job>`;
     - runs the tool headless (`claude -p …` or `codex exec …`) in the clone;
     - commits anything left uncommitted;
     - writes `result.bundle` with only the new commits, plus the transcript, to the job output.
3. **Out.** Outputs return automatically, as they do today. After delivery, the laptop's return worker imports `result.bundle` into the submitting repo as `refs/remotes/rtx/agent/<job>`. That ref is the only thing it writes; local branches and the working tree are never touched. A result whose base is no longer in the repo is kept and reported, not forced in.

## Image

`runtimes/Dockerfile.agent` builds on the locked Rust 1.98.1 image (it already has Node, npm, Python and git). It adds the Claude Code and Codex CLIs at pinned versions, through a committed npm lockfile with integrity hashes. Like every runtime, it is built on the Spark from locked sources by the bridge's installer.

## Sign-ins and their exposure

- Each tool's sign-in lives on the device under `~/sparkdev/agents/<tool>/` (mode 0700). It is mounted read-write only into that tool's agent jobs; Codex refreshes its own token.
- Claude's token reaches the job as an environment variable. It is never written to argv or to logs, and transcripts are filtered for it.
- **Exposure, stated plainly:** the agent's own build commands run in the same container as its sign-in. A malicious dependency could read it. That matches what a local agent run on the laptop exposes today. This design accepts that for v1, as recommended to the owner on 2026-10-27, rather than splitting the tool runner from the build sandbox. The owner can still say no.
- Signing out deletes the folder.

## Network

Agents need their model API. v1 requires job internet to be **on**, using the existing guarded proxy, which refuses private and link-local ranges. With internet off, agent jobs are refused with a plain message. v2 adds an allowlist mode for agents only: model API hosts plus the package registries the proxy already knows.

## Limits

An agent job reserves 4 CPUs and 8 GB by default, within the owner's budgets and client caps. Each tool also has its own concurrency cap. The agent's builds run inside its own reservation.

## Phases

- **P1 (0.7.0):**
  - Spark side: the image, the `agent` runtime, the driver, the sign-in store, and `agent login` and `agent status`.
  - Laptop side: `agent run` with bundle in and out, automatic ref import, MCP `agent_run`, and the Agents rows on the device panel.
  - Tests: unit tests with stand-in tool binaries, and an end-to-end check with stand-in tools on the Spark.
- **P2:** the agent-only allowlist network; continuing a session with a follow-up prompt; live transcript in the job view.

## To verify against the pinned CLI versions

- The headless flags and the permission modes inside a container. The container is the sandbox, so the tools' own sandboxes are bypassed there.
- The environment variable and folder each tool reads its sign-in from.
- Codex device sign-in on a machine without a browser.
- Owner acceptance needs the two real sign-ins, done by the owner.
