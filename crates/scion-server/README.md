# scion-server

The authoritative service and embeddable Axum router for [Scion](https://github.com/wieslawsoltes/scion).

The service runs SCM operations in the reusable engine, authenticates bearer credentials against hashed policy entries, enforces repository and directory permissions, signs scoped object capabilities, and serves compressed immutable objects. Request admission and upload byte reservations run before body extraction. Blocking storage work runs on Tokio's blocking pool; health, readiness and Prometheus counters support operations.

```sh
cargo run -p scion-server -- --dev --data ./scion-data
```

Anonymous development mode binds only loopback. For authenticated deployments, configure `--policy`, `SCION_CAPABILITY_KEY` and a trusted TLS boundary. Build with `--features postgres` for `--database-url` and hash-distributed `--object-shard` backends.

Applications can construct `ServerState` from any `scion_store::SharedStore` and mount `router(state)` in their own service. No working copy is required to create, inspect, merge, rebase or revert changesets. Refs move only through independent compare-and-swap requests.

See the repository's operations, security and protocol documentation. Licensed under MIT OR Apache-2.0.
