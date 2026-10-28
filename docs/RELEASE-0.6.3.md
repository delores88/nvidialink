# RTXBridge 0.6.3

This maintenance release fixes the CLI identity probe used by Connect your Spark and computer enrollment. Windows OpenSSH's `ssh-keyscan` can reject a key-exchange algorithm offered by the Spark and return no host key even though ordinary `ssh` can negotiate with the same server.

The CLI reads the ED25519 host key through authentication-disabled SSH with a fresh temporary known-hosts file. An authentication-refused exit is expected: no password or private key is offered. The temporary observation does not authorize a device. The owner must still independently compare its fingerprint, and later enrollment steps recheck the identity before password authorization or saved trust.

Errors identify an unreadable device identity and distinguish a timeout, refused connection and absent ED25519 key. Temporary observations are removed after success, timeout and process errors. The desktop UI, enrollment flow, saved pin format, paired transport and remote controller are unchanged. The separate legacy Apps-entry cleanup, WSL memory reporting and 0.7.0 agent-host work are excluded.

Release preparation starts from `70829ab713192a459d9fac3217878ab38f97dea9`, the immutable 0.6.2 product, with the reviewed post-release credential-disclosure helper and documentation checkpoints `45896065610d2782e6a697aaa54217e728a93934` and `43e97440a2c9ac64c4c990a4e5b4d8bd965d8954`. First-party reuse and unchanged UI provenance are recorded in [PROVENANCE.md](../PROVENANCE.md).

Source tests, a Windows CLI probe against the physical Spark, compiled package inspection, local installation, GitHub publication, Marketplace static admission and installed updater behavior remain separate evidence layers. Receipts will identify the actual checks completed. A static Marketplace review does not establish native installation or full application security.
