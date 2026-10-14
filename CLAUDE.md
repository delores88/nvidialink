# DGXBridge for Claude Code

Follow AGENTS.md and the repository's PhaseForge read-only boundary. For Linux-capable jobs use the installed `sparkdev` CLI with the real client label `claude`. Do not invent agent acceptance by changing that label.

```powershell
sparkdev --client claude --json status --all
sparkdev --client claude --json run C:/work/project --where auto --detach --request-id retained-unique-request -- cargo test --locked
sparkdev --client claude --json wait JOB_ID --timeout 20
sparkdev --client claude --json logs JOB_ID --since 0
```

Retain the job ID and next byte offset. Repeat bounded waits and incremental logs until terminal state, then report the actual exit code. Prepare locked dependencies with `sparkdev prepare` when needed. After a disconnect, retry the same request ID; do not start duplicate work. Use `--where laptop` for Windows-only work, preserving the unchanged shared build gate. Never bypass a host-key rejection or inspect another agent's credentials.

For native tools configure `sparkdev --client claude mcp` as documented in docs/MCP.md. Use a dedicated authorized session for acceptance, leaving active PhaseForge work untouched.
