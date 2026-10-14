# Acceptance evidence and remaining work

This preview distinguishes source tests, physical execution, installed use and
Marketplace admission. A successful build does not satisfy every layer.

## Verified physical execution

- Pinned, noninteractive SSH reached the owner's Spark. Separate controller and
  link user services were installed under `~/sparkdev`; the existing Spark Link
  and Ollama resources were preserved. [Bootstrap receipt](../evidence/ssh-bootstrap.json).
- Native Windows and Linux Spark jobs selected through auto placement ran at the
  same time and both exited 0. A competing job on the same worktree was refused.
  Windows detach returned in 0.86 seconds for a 20-second fixture job. These are
  development-binary checks, not final installed acceptance.
  [Receipt](../evidence/cli-laptop-regression.json).
- Exact argv was exercised from PowerShell, cmd and Git Bash. Spark result reuse,
  request idempotency and incremental logs were exercised; the original receipt
  also retains a detach failure subsequently fixed by the laptop regression.
  [Initial CLI receipt](../evidence/cli-spark-acceptance.json).
- Locked dependency preparation followed by offline Rust, Node and Python jobs,
  CUDA execution, and a controller restart during an existing job were exercised
  on the physical Spark. The restart preserved the existing run rather than
  starting another container. [Dependency/restart receipt](../remote/receipts/dependencies-restart-physical.json),
  [runtime receipt](../remote/receipts/runtime-physical.json).
- The ARM64 release CLI passed 23 tests, real devices/plan commands and an eight
  tool MCP protocol smoke test. The container's device status correctly reported
  that it had no owner connection credentials. This is not proof of an actual
  Claude Code or Codex model calling MCP.
  [Linux CLI receipt](../remote/receipts/linux-arm64-cli-final.json).
- Fresh Claude Code and Codex sessions each called devices, plan, run, wait and
  logs using the installed CLI. Their two Spark jobs overlapped, both succeeded
  with exit 0, and both returned complete fixture logs. Invocation-only approvals
  were scoped to the authorized MCP tools. The combined harness still correctly
  failed because the separate Codex restricted Windows shell check reported
  missing writable-root capability SIDs. No global security setting was changed.
  [Real session receipt](../evidence/real-agent-mcp.json).
- The Spark recorded phone approval and device approval at 02:51:10 UTC on
  October 14, before the phone confirmation UI update. This records accepted
  pairing, without identifying the physical approver or proving the new animation.
  [Sanitized pairing receipt](../remote/receipts/owner-pairing-before-phone-update.json).
- The final DGXBridge Windows installer completed without elevation; the installed
  payload matched the release inputs, allowing only Tauri's documented three-byte
  NSIS bundle marker change. Installed doctor confirmed authenticated LAN SSH and
  a healthy scheduler. Native inspection showed successful pairing confirmation,
  six jobs per page, working next/previous navigation and retained page position.
  [Package receipt](../evidence/dgxbridge-installed-package.json),
  [native receipt](../evidence/dgxbridge-native-ui.json).

## PhaseForge benchmark

An isolated archive of `ac30e04fbf0ddc03ae0144fb06b6350cacdce1fc` was run through
DGXBridge with 8 CPUs and 32 GiB. PhaseForge's checkouts and source were
not edited. Dependencies were prepared before the empty target-cache run.

| Phase | Cargo compile time | Entire Spark job | Test totals |
| --- | ---: | ---: | --- |
| Empty target cache | 102 s | 183.16 s | 1968 passed, 19 failed, 75 ignored |
| No source change | 0.16 s | 80.73 s | 1968 passed, 19 failed, 75 ignored |
| One comment changed in isolated source | 24.04 s | 104.54 s | 1968 passed, 19 failed, 75 ignored |
| Larger owner-granted scratch space | 0.16 s | 80.68 s | 1969 passed, 18 failed, 75 ignored |

The last run increased only this benchmark's temporary storage allowance from
512 MiB to 4 GiB, still charged against its memory limit. Seventeen remaining
failures share an ARM64-incompatible file-open flag path; the other test requires
Windows. These are failed-suite timings, not claims of a successful backend
suite. [Detailed findings](../NOTES_FOR_CLAUDE.md),
[timing receipt](../remote/receipts/phaseforge-benchmark.json),
[corrected scratch receipt](../remote/receipts/phaseforge-scratch-corrected.json).

The resumed upload transferred 73,525,917 bytes in 62.66 seconds, or 79.95 seconds
including inventory and local overhead. It resumed an intentionally interrupted
CLI upload; it is neither a fresh full-upload benchmark nor a physical network
drop test. No-change transfer/materialization was approximately 1.7-2.0 seconds;
the one-comment change uploaded 2220 bytes in 2.39 seconds.
[Resumed sync receipt](../remote/receipts/benchmark-sync-resumed.json).

The first laptop baseline never compiled because its isolated fixture was
nested inside this repository's Cargo workspace. That failed attempt is retained
in `evidence/phaseforge-laptop-cold.txt`. A separate fixture outside the workspace
compiled in 15 minutes 23 seconds through the unchanged mandatory build gate.
The bridge's own comparison job was then cancelled during tests to release
laptop resources. It exited 130, so there are no completed laptop test counts or
fair end-to-end comparison. [Interrupted baseline receipt](../evidence/phaseforge-laptop-corrected-interrupted.json).

## Not yet accepted

- Direct taskbar inspection; the installed executable/title bar and Start-menu
  shortcut contain the DGXBridge icon, but no taskbar screenshot was captured.
- Actual phone inspection of the updated approval confirmation. The prior real
  pairing receipt and synthetic confirmation tests establish separate layers.
- Codex restricted Windows shell execution: observed failure on this laptop,
  distinct from the successful actual MCP workload acceptance above.
- Phone-hotspot remote execution through an owner VM. No deployed owner relay
  was found; explicit opt-in public relay support is not owner-VM acceptance.
- Physical network interruption during transfer and execution, and actual laptop
  sleep while a job runs. A restarted controller and interrupted CLI are different
  failure modes.
- Marketplace admission and authenticated Marketplace download verification.
  Beta.1 was rejected for two Gitleaks matches in a source-hash receipt. Both
  values were verified against the exact tagged source bytes as SHA-256 checksums,
  not credentials. Malware scanning passed; dependency scanning passed with no
  high or critical findings and one medium Linux GLib advisory. The GitHub beta and exact generated manifest
  are published; anonymous installer/manifest downloads match their hashes.
  Actual installed screenshots and artwork passed local validation against the
  deployed Marketplace contract. After the owner foregrounded Marketplace,
  authenticated native validation, draft save and submission completed. Release
  `rR8U9MLCyJmHpFHVGF7Fxxel` is rejected; no eligible Marketplace artifact has been
  confirmed yet. Native progress updates reported `NETWORK_UNAVAILABLE`, but the
  Release decisions findings and public catalog agree on rejection. The publisher identity
  and existing read-only GitHub App repository authorization were verified.
  [Published release receipt](../evidence/dgxbridge-public-beta.json),
  [manifest validation](../evidence/dgxbridge-manifest-validation.json).
  Browser and native login state are separate. No featured listing is requested.

## Current functional limits

The shared scheduler lives on Spark. New native laptop jobs require its lease;
offline local scheduling is not implemented. Local result reuse is disabled
because a mutable native toolchain has no locked runtime digest. Auto placement
recognizes explicit Windows commands/targets but cannot infer every script's
requirements. First source synchronization happens before detached job admission
and can exceed short agent tool timeouts on large trees. See [MCP.md](MCP.md).

The CPU runtime's base image and Python wheels are pinned. Debian packages are
resolved from their repositories during owner setup; the resulting local image
ID is then locked in the runtime registry. This is not a reproducible snapshot of
every Debian package download. Native laptop commands run with owner permissions.
