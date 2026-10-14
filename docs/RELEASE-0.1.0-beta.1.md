# DGXBridge 0.1.0-beta.1

First public MIT beta of the Windows desktop app, `sparkdev` CLI and stdio MCP
server for an owner-controlled NVIDIA DGX Spark. This is an ordinary Marketplace
utility; PhaseForge remains the featured application.

The installer includes the GUI, Windows CLI/link sidecar, ARM64 bridge service,
bootstrap bundle, license and third-party notices. It installs for the current
Windows user without elevation. Preinstalled WebView2 and OpenSSH are required.
The beta installer is unsigned. The repository also supplies a Linux ARM64 CLI.

Verified on Windows 11 Pro 10.0.26200 with 32 GB installed RAM and the physical
ARM64 Spark: authenticated LAN SSH, successful device pairing, native paginated
job board, Windows/Spark auto placement, worktree claims, resumable source chunks,
Spark result reuse, exact argv from three Windows shells, offline prepared
workloads, and controller restart during an existing run. Two fresh real Claude
Code and Codex sessions submitted overlapping Spark jobs through MCP; both exited
0 with complete logs. See [acceptance](ACCEPTANCE.md) for exact evidence and scope.

Observed failure: the separate Codex restricted Windows shell check reported
`workspace-write sandbox has no writable root capability SIDs` on this laptop.
The MCP concurrency test passed independently. No global sandbox or security
configuration was changed. The combined acceptance harness retains its failure.

Unverified: owner-VM/hotspot operation, physical network loss during transfer/run,
laptop sleep during a run, and direct phone inspection of the updated confirmation
animation. The desktop's final pairing confirmation was observed. The isolated
PhaseForge benchmark still has upstream/platform test failures and its laptop
comparison was interrupted; those timings are not a passing-suite comparison.

Known limits: new laptop jobs need Spark's shared scheduler; native laptop jobs
run with owner permissions and do not use a result cache. Initial source sync can
exceed short agent tool deadlines. Runtime setup pins OCI bases and Python wheels,
then resolves Debian packages and locks the resulting image ID. It is not a fully
reproducible snapshot of every Debian package. Runtime provisioning is outside the
Marketplace installer/source scan. State and logs have no automatic cleanup.

Install the supplied `DGXBridge_0.1.0-beta.1_x64-setup.exe`, or use its `/S` option
for a silent current-user installation. Open DGXBridge from the DeloresAI Start
menu folder. The Agent setup page shows the actual `bin/sparkdev.exe` path and
MCP configuration. [CLI usage](CLI.md) and [agent setup](MCP.md) cover retained job
IDs, bounded waits and offset-based logs. Uninstall preserves bridge state and
remote services.

Marketplace admission and download verification are separate release gates;
the presence of these notes or a GitHub asset does not establish admission.
