# Link adapter implementation review

Scope: `crates/sparkdev-link`, its copied protocol primitives, and its fixed RPC seam. This is the implementation agent's source review and test evidence, not an independent security audit.

## Boundaries checked

- The node has its own state, identity, service name, UDP port and temporary phone listener. No code invokes PhaseForge's owner bootstrap, service, job runner, Ollama API or state paths.
- Only the same Unix user can request an owner ticket through the protected local socket. The ticket is bound to the public client identity received over the existing pinned SSH connection.
- The reused request and acceptance MACs bind both identities, the ticket, expiry and the TLS exporter. The phone token is derived from the secret; the expected six-digit code is displayed only on the laptop. Approval is neither inferred nor automatic.
- Device authorization precedes accepting an RPC stream and is checked again before dispatch. The only server subprocess target is `python3 ~/sparkdev/controller.py rpc`, with structured stdin. This adapter does not admit arbitrary server commands or Docker images.
- Temporary phone HTTP checks host, origin, framing and body limits; external assets, caching and referrers are disabled. Tickets expire, bad token/code attempts burn them, and cancellation or connection loss closes approval.
- Windows keys use user-scoped DPAPI with machine-derived entropy. State resolves to the unredirected known folder; Unix keys and state are owner-only. Symlink/reparse state is rejected. Identity creation and endpoint use are protected by OS locks.
- A relay concurrency issue was found and corrected: separate processes sharing one iroh identity could replace each other's relay connection. The endpoint lease now serializes short RPC sessions, with bounded `link_busy` retry and short wait polling. Remote jobs remain concurrent.
- Audit entries omit tickets, tokens, codes, keys and request bodies. An approval is audited durably before adding its device to the persisted allowlist.

## Verification

The physical ARM64 Spark passed 76 tests, including nine adapter tests. Windows passed nine adapter tests, including DPAPI persistence and the unredirected known folder. The integration tests use ephemeral synthetic identities: one confirms unauthorized RPC rejection, and one validates the full pairing MAC exchange and persisted allowlist after a synthetic phone decision. See adjacent JSON and text receipts. These tests did not approve the installed owner's identity.

## Limits retained

- Phone approval uses the requested private-LAN HTTP flow. It assumes that local network is trusted; the six-digit comparison remains an explicit human step.
- No deployed owner VM was found. Relay defaults to disabled; public n0 is an explicit opt-in. Custom HTTPS relay addresses are supported, but custom relay shared-token authentication is not implemented.
- A real phone approval, off-LAN hotspot execution and native installed UI acceptance are separate workflow checks. Their success cannot be inferred from these tests or compilation.
- The bridge uses its own controller for durable scheduling. That controller, dependency proxy, installer and desktop UI have their own review and acceptance scope.
