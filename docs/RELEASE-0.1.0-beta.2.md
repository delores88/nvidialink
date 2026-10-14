# DGXBridge 0.1.0-beta.2

Replacement for the rejected beta.1 Marketplace submission. Two Gitleaks matches
were verified as deterministic SHA-256 hashes of the exact tagged source files,
not credentials. The ARM64 build receipt now records paths and hashes as separate
fields, retaining all 46 original pairs and the original execution evidence.
No scanner rules, suppression lists or Marketplace policy were changed.

This release also addresses the owner's report that the installed app's logo was
missing from the Windows taskbar. It assigns the Windows process identity to the
same ID as the installed shortcut and explicitly sets the large taskbar icon from
the embedded multi-size ICO. Startup checks that Windows retained the icon. The
title-bar icon keeps its existing framework-managed lifetime. Installed visual
verification is pending; the owner's taskbar auto-hides by default.

The CLI, link sidecars and remote controller behavior remain the same as beta.1.
Previously verified physical Spark jobs and real Claude Code/Codex MCP acceptance
remain evidence for those unchanged components, not new beta.2 runs.

The beta is MIT, unsigned, installed per user, and requires preinstalled WebView2
and OpenSSH. It is an ordinary nonfeatured utility; PhaseForge remains featured.
All [beta.1 functional limits and incomplete acceptance](RELEASE-0.1.0-beta.1.md)
still apply, including the separate Codex restricted Windows shell failure.

Marketplace admission is pending until the new immutable release's automated
checks accept its exact artifact. A five-minute thread heartbeat checks Developer
tools > Release decisions, fixes rejection, and disables itself after acceptance.
