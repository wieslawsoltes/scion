# Operating Scion

## Deployment units

Run `scion-server` as the authority, `scion-cache` as an optional regional cache, and `scion-daemon` as an optional shared node cache. The client sends control queries and mutations to the authority. It sends immutable-object requests to its configured object endpoint, which can be a daemon, cache, or authority.

Default TCP ports are 7420, 7421, and 7422 respectively. Service inspection endpoints are:

| Route | Purpose |
|---|---|
| `GET /health` | Process liveness |
| `GET /ready` | Authority readiness |
| `GET /v1/info` | Protocol version, service version, and tier |
| `GET /metrics` | Operational counters |

Use `--help` on the installed executable for its complete option set. Direct HTTP is suitable on loopback or a trusted internal network. Terminate TLS before exposing services across an untrusted boundary.

## Local evaluation with containers

```sh
export SCION_CAPABILITY_KEY="$(openssl rand -hex 32)"
export SCION_TOKEN="$(openssl rand -hex 32)"
docker compose up --build --detach --wait
export SCION_SERVER=http://127.0.0.1:7420
export SCION_OBJECTS=http://127.0.0.1:7422
export SCION_REPO=demo
scn init --author "Operator <operator@example.test>" --message "Initialize"
scn clone main work
```

The [Compose topology](../compose.yml) publishes host ports only on loopback, isolates the authority and node networks, and uses separate persistent volumes for each tier. A one-shot setup container hashes `SCION_TOKEN` and creates a policy granting the operator full access to the `demo` repository. The authority mounts that policy read-only. Generate stable credentials for a persistent evaluation environment; all three tiers must use the same capability key. Keep credentials in a secret store or untracked environment configuration.

Stop the services with `docker compose down`. Add `--volumes` only when you intend to erase the evaluation data.

## Authenticated authority

The authority requires an explicit development mode or an access policy. A production policy maps token hashes to principals and repository permissions. Hash a credential using the server helper instead of storing its plaintext in the policy:

```sh
# Set SCION_TOKEN through your credential manager.
scion-server --hash-token
```

Example `policy.json`:

```json
{
  "principals": [
    {
      "name": "frontend-agent",
      "token_hash": "REPLACE_WITH_HASH_FROM_THE_HELPER",
      "repositories": {
        "demo": {
          "read": true,
          "write": true,
          "admin": false,
          "rules": [
            { "prefix": "secrets", "read": false, "write": false },
            { "prefix": "generated", "write": false }
          ]
        }
      }
    }
  ]
}
```

Rules use component-aware prefixes and longest-prefix selection. An omitted permission inherits the repository or applicable parent policy. `src` does not match `src-other`. Policy files are operator configuration, not repository-controlled content.

```sh
export SCION_CAPABILITY_KEY="$(openssl rand -hex 32)"
scion-server \
  --listen 127.0.0.1:7420 \
  --data ./scion-data \
  --policy ./policy.json
```

The client reads its bearer credential from `SCION_TOKEN`. Working-copy state stores the server, object endpoint, and repository identity, but does not store the token.

### Directory permissions and broad queries

Path-scoped reads enforce the applicable directory policy. Broad operations can reveal filenames, changeset metadata, or history outside a permitted directory. The current authority therefore denies broad metadata operations to principals without full repository read access instead of returning a partially reconstructed repository view. Merge, rebase, and revert require repository-wide permissions.

Blob upload provenance is scoped to the principal and repository. Knowing an object ID from another repository is insufficient to introduce that object into a commit. Object tickets are issued only after an authorized revision/path witness is checked.

### Capability keys

The authority signs five-minute object capabilities with a 32-byte key, represented as 64 hexadecimal characters. Regional and node caches verify the same signature. Changing any ticket field invalidates its signature.

Capabilities can remain usable until their expiry after a policy change. If immediate invalidation is required, rotate the signing key across the authority and caches. Treat all holders of the key as trusted infrastructure operators: any holder can mint tickets. Bearer credentials and capability keys have different roles and should not share values.

## Cache tiers

Start a regional cache:

```sh
scion-cache \
  --listen 127.0.0.1:7421 \
  --data ./regional-cache \
  --upstream http://127.0.0.1:7420 \
  --capability-key "$SCION_CAPABILITY_KEY" \
  --max-bytes 1073741824
```

Start a node daemon:

```sh
scion-daemon \
  --listen 127.0.0.1:7422 \
  --data ./node-cache \
  --upstream http://127.0.0.1:7421 \
  --capability-key "$SCION_CAPABILITY_KEY" \
  --max-bytes 536870912
```

An optional Unix socket can be selected with `--socket /path/to/scion.sock`; Unix clients select it with `SCION_OBJECTS=unix:///path/to/scion.sock`.

The configured upstream must have the expected tier: daemon → cache → server. An upstream failure must not cause a daemon to bypass its cache and contact the authority. This preserves the failure boundary and prevents an outage from concentrating the entire node load on the authority.

After five consecutive upstream failures, the cache opens its circuit breaker for five seconds. When that cooldown expires, one probe checks whether the upstream has recovered before normal traffic resumes. The `scion_cache_circuit_rejections_total` metric distinguishes requests rejected during that interval from requests that reached the upstream. Concurrent misses for the same object share one upstream fetch, and `--max-inflight` bounds concurrent upstream work.

Cache byte budgets govern stored object bytes. Leave additional disk space for metadata, temporary files, and snapshots. A useful initial budget comes from the working set of overlapping jobs on the node, not from the complete repository size.

### Snapshots

```sh
scn cache export ./node-cache ./node-cache.snapshot
scn --cache-bytes 536870912 cache import ./new-node-cache ./node-cache.snapshot
scn cache stats ./new-node-cache
```

A cache directory is exclusively owned by its running process. Stop the service before opening that directory with the standalone `scn cache` commands. Use the service's snapshot options for ongoing snapshots:

```sh
scion-daemon \
  --data ./node-cache \
  --upstream http://127.0.0.1:7421 \
  --capability-key "$SCION_CAPABILITY_KEY" \
  --snapshot ./warm.snapshot \
  --snapshot-out ./next.snapshot \
  --snapshot-interval 300
```

`--snapshot` imports a snapshot at startup. `--snapshot-out` writes a snapshot during clean shutdown and periodically while the daemon runs. `--snapshot-interval` is in seconds and defaults to 300; setting it to `0` disables periodic writes while retaining the shutdown snapshot. The regional cache accepts the same snapshot options. Snapshot import validates object framing, sizes, and content hashes.

A cache snapshot warms a disposable cache. It does not back up repository references, change logs, access policy, or the authoritative object collection.

## Persistence and scaling

The local backend stores compressed immutable objects on disk and metadata in SQLite. It is convenient for a single authority and deterministic tests. SQLite serializes writes; this backend does not claim distributed write scalability.

Build the server with PostgreSQL support for shared transactional metadata:

```sh
cargo build -p scion-server --release --features postgres --locked
```

Configure `--database-url` with a PostgreSQL connection string. TLS connections use Rustls with the platform's trusted root certificates. Additional `--object-shard` connection strings select independent object-storage shards. Instances sharing the same repository authority must use compatible metadata and shard configuration. Keep the shard layout stable: changing a hash-to-shard mapping requires moving the existing objects or providing a migration layer.

The storage interface separates immutable object operations from mutable metadata and reference CAS. A persistent backend must preserve durability, idempotence, and atomic reference updates; sharing only the object directory is not a substitute for shared transactional metadata.

### Change-log ordering

Changeset identity is independent of the change-log sequence. Concurrent PostgreSQL transactions can reserve sequence numbers and commit in a different order. Log writers use a shared advisory gate, and a log reader briefly takes the exclusive gate to establish a committed sequence frontier. This prevents a consumer from advancing its cursor past a transaction that has not committed yet.

The repository engine does not acquire a repository-wide mutation mutex or move a ref when creating immutable commits. The durable backends still use transactions and storage-level coordination. Calling the entire deployment strictly lock-free would be inaccurate.

## Recovery

Back up authoritative metadata and objects as a consistent set. Preserve policy configuration and the signing key separately according to the organization's secret-management process. Immutable objects may be copied incrementally, but references and derived-data mappings need a consistent restore point.

After restoring, start an isolated authority, check known refs and representative changesets, and materialize selected files to verify hashes and modes. Rebuild disposable node and regional caches from the authority or validated cache snapshots. Do not substitute cache data for missing authoritative history.

## Diagnostics

Record the service version and protocol version with incident reports. Monitor request errors and latency, cache hit/miss/eviction counts, upstream failures, object sizes, memory, disk usage, and backend latency. Inspect cache tiers independently: a warm regional cache can hide an unexpectedly cold node cache.

When a reference update conflicts, reload the current value and decide whether to rebase or merge before retrying. Do not turn an expected-value failure into an unconditional overwrite. When integrity verification fails, preserve the object ID and failing tier, then compare with the authority; replacing an invalid cache entry is different from repairing authoritative corruption.
