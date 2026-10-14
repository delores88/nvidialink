# CLI provenance

The Rust CLI is a first-party implementation. Its source/secret policy and SSH transport design were translated from the owner's read-only `D:/PhaseForge-claude/scripts/sparkbuild.py` reference, SHA-256 `8c6ad28dc5cd81ef903fe9a27e524479e40d2ad357ef8a50759964631b1a5b3f`, inspected 2026-10-13. The referenced PhaseForge Spark Link protocol revision is `ac30e04fbf0ddc03ae0144fb06b6350cacdce1fc`.

Destinations: `crates/sparkdev/src/inventory.rs` (deny patterns, Git tracked/untracked inventory and out-of-tree refusal) and `transport.rs` (strict SSH admission and pluggable transport concept). Implementation changes include Rust file/path handling, whole-tree validation before transfer, SHA-256 1 MiB CAS objects, structured JSON requests, no tar extraction, no hardcoded owner addresses or credential paths, and separate application state. The prototype's remote resource names, unrestricted networking and mutable PhaseForge directories were not imported.

The owner-authorized MIT grant is recorded in `PROVENANCE.md`. Dependency licenses remain their own. No source, branch, service or runtime in PhaseForge was modified.
