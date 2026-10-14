# DGXBridge

This repository alone is writable. All PhaseForge repositories, worktrees, release directories and Spark services are read-only references. Never mutate `D:/PhaseForge-claude`, except invoking the owner's prescribed build gate as-is for local cargo work. Never alter `~/pf-builds`, `~/phaseforge-runtimes`, `~/phaseforge-test-node`, their containers, images or volumes.

Read `docs/DESIGN.md` before implementation. Use small reviewed commits. Record reused code and exact upstream commit in `PROVENANCE.md`. Never commit credentials, private keys, pairing tickets or raw owner configuration. Local state belongs outside the repository.

Local cargo builds/tests must run through `python -I D:/PhaseForge-claude/scripts/buildlock.py -- cargo ...`. Keep build artifacts on C: (D: has limited free space). Do not start unrestricted Spark jobs: use separate sparkdev resources and caps. Never sudo.

Receipt claims must distinguish unit tests, real Spark runs, installed Windows behavior, remote hotspot verification and Marketplace admission. Never fabricate a Claude Code run by setting `--client claude` yourself.

For agent jobs, use `sparkdev --client codex --json status --all`, then `sparkdev --client codex --json run DIR --where auto --detach --request-id RETAINED_ID -- COMMAND ARGS`. Retain the returned job ID, poll `wait JOB --timeout 20`, and consume `logs JOB --since OFFSET` using its next byte offset. Inspect the final workload exit code. Retry an uncertain submission with the same request ID instead of duplicating it. Prepare locked dependencies before offline execution; see docs/CLI.md and docs/MCP.md. Keep Windows-only jobs on the laptop through the unchanged build gate.
