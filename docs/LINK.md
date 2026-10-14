# Spark Dev Bridge owner link

This independent iroh bridge uses Spark Link's pinned cryptographic primitives, with its own endpoint identities, state, ALPNs and services. It does not contact PhaseForge's node or Ollama. See `PROVENANCE-LINK.md` for the upstream boundary and license.

## Layout and bootstrap

On the Spark install the ARM64 `sparkdev-link` binary to `~/sparkdev/bin/sparkdev-link`. The distinct user unit `sparkdev-link.service` runs that binary with `serve`. Its state is `~/sparkdev/link` (0700), persistent identity and device allowlist files are 0600, and its owner-only control socket is `~/sparkdev/link/control.sock`. It listens on UDP 47831. The temporary phone approval page binds only the selected private LAN IP on TCP 7437. `SPARKDEV_LAN_IP` may select the Spark's wired or Wi-Fi private address; the automatic choice comes from the operating system's route table. A node with no private LAN address refuses startup. The node calls only the fixed `python3 ~/sparkdev/controller.py rpc` helper for authenticated job requests.

The client root is `SPARKDEV_HOME/link`, otherwise `%LOCALAPPDATA%/SparkDevBridge/link` on Windows or `~/.local/state/sparkdev/link` on Linux. Windows device keys use user-scoped DPAPI with MachineGuid-derived entropy: a copied profile on another machine cannot silently reuse the identity. Linux keys are owner-only files. Key creation uses an OS lock and rereads the durable file. Client processes never print private key material.

The SSH bootstrap must use the CLI's strict host-key-pinned BatchMode connection. It deploys only the distinct binary/unit above, verifies uploaded SHA-256, and starts only `sparkdev-link.service`. Do not call upstream `spark-link-agent link.owner_pair.start`: that implementation deliberately targets PhaseForge's service and paths.

## Pairing integration

1. Local `sparkdev-link identity` returns `{ok:true,device_id}`. This is a public endpoint identity, safe to pass as an SSH argument after parsing/validation.
2. Over the pinned SSH connection run `~/sparkdev/bin/sparkdev-link owner-ticket --device-id <device_id> --device-name <label>`. Its JSON result contains `{ok:true,ticket:{...}}`. Capture it privately: do not print, log or save it.
3. Start local `sparkdev-link pair`, pass the complete owner-ticket response on stdin, then close stdin. The child emits JSON lines. The first is `{ok:true,state:"pending",qr_url,qr_svg,verification_code,expires_at,node_id}`. Present this event transiently in the trusted desktop UI. Neither QR token nor code belongs in logs, receipts, screenshots, crash reports or saved state.
4. The owner scans the QR with the phone camera, enters the six-digit code displayed on the laptop and taps Approve. The phone needs no app and gets no Spark credentials. The child then emits `{ok:true,state:"paired",node_id}` and exits. Only then is the authenticated node record written locally.
5. Deny, expiry, wrong-code lockout or connection closure grants no new device. Terminating the client closes pairing after QUIC detects loss (12-second idle timeout); the temporary listener closes. For immediate cancellation, run `~/sparkdev/bin/sparkdev-link owner-cancel --device-id <device_id>` over the same pinned SSH connection. Tickets expire after 300 seconds and are memory-only. Node restart invalidates all outstanding tickets. A pending ticket must expire or be cancelled before another can be minted.

Ticket derivation, TLS exporter transcript binding, constant-time request/accept MAC validation, approval-token derivation and six-digit SAS conversion are reused unchanged from the pinned upstream protocol. The new protocol additionally binds the ticket to the exact public device identity supplied through SSH. The node never transmits the expected approval code in the page or QR. Five bad token/MAC attempts or three wrong codes burn the ticket. The token travels in a URL fragment, is cleared from the phone address bar, and is submitted only with the same-origin decision. HTTP framing, host, origin and body size are checked; the page has no external assets and uses restrictive CSP/no-store/no-referrer headers.

## Agent calls and remote policy

`sparkdev-link rpc` reads `{request:{v:1,op:...}}` on stdin and returns the controller's flat JSON response. The paired node endpoint is pinned, and the node checks the device allowlist before accepting application streams and before RPC dispatch. Maximum frame size is 4 MiB; connection timeout is 12 seconds and RPC wait is bounded. The main `sparkdev` CLI chooses SSH on the LAN and uses this adapter only for an already paired remote route. A failed SSH host-key check must never trigger fallback.

`sparkdev-link status` (also `pair-status`) returns linked/reachable state and public node identity, without credentials. `identity` and `status` are noninteractive. `pair` is explicitly the desktop human-approval operation; ordinary agent jobs never prompt.

Relay defaults to `{"mode":"disabled"}`. The Spark's owner-only `~/sparkdev/link/relay.json` can select `{"mode":"n0_public"}` or `{"mode":"custom","urls":["https://owner-relay.example"]}` before starting this distinct service. Custom relay URLs require HTTPS and prohibit embedded credentials. Shared-token relay authentication is not implemented in this adapter. Public n0 is an explicit beta opt-in, not evidence of an owner VM. The upstream protocol and release agent report no deployed owner relay; no VM deployment is claimed here. Relay servers see metadata, not decrypted job content. No address lookup services, UPnP router changes or 0-RTT are enabled.

## Evidence limits

Windows source validation passed 66 upstream protocol/vector tests and six adapter tests on 2026-10-13 using the prescribed shared build gate. Adapter coverage includes actual iroh rejection of an unpaired endpoint, DPAPI identity persistence, token/code burn limits and single-use denial. Protocol tests and compilation establish source behavior only. A real owner phone approval, off-LAN hotspot run and installed native acceptance require separate receipts. Service restart does not stop Docker jobs because the job controller owns durable job reconciliation; link pairing records survive, but outstanding tickets intentionally do not. The bridge does not reuse upstream's single-slot job runner or its memory-only event stream.
