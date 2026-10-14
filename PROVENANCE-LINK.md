# Spark Dev Bridge link provenance

Owner instruction on 2026-10-13 explicitly authorizes reuse of PhaseForge Spark Link in this new repository and prohibits any PhaseForge modification. All source extraction was read-only `git show` at a fixed commit.

Upstream repository: https://github.com/delores88/PhaseForge

Pinned revision: `ac30e04fbf0ddc03ae0144fb06b6350cacdce1fc` (`claude/0.25-spark-p2` reference supplied by the owner).

The complete `native/spark-link/crates/spark-link-proto` source and test vectors are vendored under `crates/sparkdev-link/vendor/spark-link-proto`. Source files are unchanged; `Cargo.toml` replaces workspace dependency references with their exact upstream pins and records this product's MIT grant. SHA-256 file receipts are in `crates/sparkdev-link/vendor/SOURCE-HASHES.json`.

On 2026-10-13 Ben Van Dorn, the copyright owner, explicitly directed that Spark Dev Bridge and CodaClauda be MIT and publicly available through the Marketplace. His instruction separately grants MIT rights to the copied owner-authored Spark Link sources in this product. `vendor/spark-link-proto/LICENSE` is the effective MIT license, retaining Ben Van Dorn's attribution; `NOTICE.md` records this scoped grant. It does not change the PhaseForge repository or relicense third-party dependencies. Upstream at the pinned commit used PhaseForge Free Use License 1.0 / `LicenseRef-Delores-Proprietary`; those historical terms are not the license of these copies under the owner's new grant.

Reused by the adapter: pairing HKDF/HMAC and TLS exporter transcript, constant-time MAC validation, approval-token derivation, six-digit SAS code derivation, canonical identifiers/secret wrappers, framed JSON serialization, private-LAN validation, and upstream test vectors. The Rust QR renderer follows `spark-link-agent/src/pairing.rs::qr_svg` using the same pinned qrcode crate. Windows DPAPI handling follows the same platform API pattern, with separate files and MachineGuid supplied as DPAPI entropy.

New adapter: `crates/sparkdev-link/src/` owns its independent identity paths, ALPNs `delores/sparkdev/pair/1` and `delores/sparkdev/rpc/1`, owner-ticket socket, approval page, allowlist, client commands and fixed controller dispatch. The wrapper is not Spark Link wire-compatible and never joins the existing PhaseForge node. No upstream service installer, role switching, Ollama proxy or job scheduler is executed.

Important upstream limitations recorded during review: `jobs.rs` at this revision admits one running job; its restart recovery removes active containers and records an interrupted failure. `job_events.rs` events are memory-only. Its runtime registry admits only python_ml and Blender. Spark Dev Bridge's durable build scheduler therefore lives in its own controller rather than claiming these upstream components implement concurrent build/restart acceptance.
