# scion-protocol

Typed version 1 control messages and immutable-object authorization for [Scion](https://github.com/wieslawsoltes/scion).

`RpcRequest` describes server-side commits, refs, graph queries, file queries, diff, merge, rebase, revert, bisect, copy detection and paginated manifests. `RpcReply` carries a typed JSON result or structured error. The protocol crate does not depend on an HTTP stack or async runtime.

All immutable-object tiers accept the same `ObjectRequest`. A short-lived `ObjectTicket` binds the repository, changeset, path, object kind, ID and expiry under a keyed BLAKE3 MAC. A content hash alone never authorizes an object read. Infrastructure operators provision a shared 32-byte key; clients receive signed tickets after the authority validates a path witness.

`Tier` expresses the enforced server → regional cache → node daemon topology. `ServiceInfo` makes the protocol version and tier available for startup validation. See the repository protocol document for endpoints, envelopes, limits and independent-format choices.

Licensed under MIT OR Apache-2.0.
