# DGXBridge

This repository alone is writable. All PhaseForge repositories, worktrees, release directories and Spark services are read-only references. Never mutate `D:/PhaseForge-claude`, except invoking the owner's prescribed build gate as-is for local cargo work. Never alter `~/pf-builds`, `~/phaseforge-runtimes`, `~/phaseforge-test-node`, their containers, images or volumes.

Read `docs/DESIGN.md` before implementation. Use small reviewed commits. Record reused code and exact upstream commit in `PROVENANCE.md`. Never commit credentials, private keys, pairing tickets or raw owner configuration. Local state belongs outside the repository.

Local cargo builds/tests must run through `python -I D:/PhaseForge-claude/scripts/buildlock.py -- cargo ...`. Keep build artifacts on C: (D: has limited free space). Do not start unrestricted Spark jobs: use separate sparkdev resources and caps. Never sudo.

Never run `sparkdev discover`: Find my Spark is an owner-only action in the desktop app and not an MCP tool.

Receipt claims must distinguish unit tests, real Spark runs, installed Windows behavior, remote hotspot verification and Marketplace admission. Never fabricate a Claude Code run by setting `--client claude` yourself.

For agent jobs, use `sparkdev --client codex --json status --all`, then `sparkdev --client codex --json run DIR --where auto --detach --request-id RETAINED_ID -- COMMAND ARGS`. Retain the returned job ID, poll `wait JOB --timeout 20`, and consume `logs JOB --since OFFSET` using its next byte offset. Inspect the final workload exit code. Retry an uncertain submission with the same request ID instead of duplicating it. Prepare locked dependencies before offline execution; see docs/CLI.md and docs/MCP.md. Keep Windows-only jobs on the laptop through the unchanged build gate.

The owner approved the 0.4.0 candidate's default-on public job internet with home/private networks blocked. It is proxy-based, not direct raw-socket access. This approval does not authorize agents to change the owner's saved internet or resource policy later. Spark jobs save downloads in `/cache`, shared by all jobs; keep the default cache settings and put reusable large files in `/cache/files`. `--no-cache` still uses saved downloads. Do not store credentials there or change per-job token/code-cache paths. Only the owner clears or changes the shared download budget. For a reusable HTTP model, use `serve` and `call` with retained request IDs; read docs/SERVERS.md. Source implementation, local tests, physical Spark validation, installed behavior and Marketplace acceptance remain separate claims.
