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

The authority requires an explicit development mode or an access policy. A production policy maps static token hashes or verified OIDC issuer/subject pairs to principals and repository/directory permissions. See [identity federation and policy reload](identity.md) for provider configuration, signing-key rotation, and atomic replacement. Hash a credential using the server helper instead of storing its plaintext in the policy:

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

Rules use component-aware prefixes and longest-prefix selection. An omitted permission inherits the repository or applicable parent policy. Repository permissions are defaults: a default-deny principal can receive explicit read/write grants for selected directories. `src` does not match `src-other`. Policy files are operator configuration, not repository-controlled content.

```sh
export SCION_CAPABILITY_KEY="$(openssl rand -hex 32)"
scion-server \
  --listen 127.0.0.1:7420 \
  --data ./scion-data \
  --policy ./policy.json
```

Policy files reload every ten seconds by default. Set `--policy-reload-seconds 0` to disable polling. Invalid replacements retain the last valid policy; a valid empty principal list revokes all authenticated principals. Policy distribution across several authorities remains an operator responsibility.

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

## Resource limits

The authority defaults to 128 admitted requests and 10,000 requests per second with a burst of 20,000. Configure `--max-concurrent` and `--requests-per-second` for the available memory and backend capacity. Admission starts before request-body extraction and stays reserved while blocking backend work or successful RPC/object response data remains live. Slow readers therefore occupy capacity until their response data is released. Saturated admission and rate limits return a structured `429` response.

Blob uploads have a 256 MiB object ceiling and share a 1 GiB reservation pool. Length-delimited uploads reserve their advertised size, rounded up to MiB; chunked uploads reserve the full object ceiling. RPC bodies and serialized results are limited to 16 MiB. Use file pagination and immutable-object APIs for larger results. These limits bound individual operations; select concurrency with both compressed response buffers and temporary decoded objects in mind.

## Persistence and scaling

The local backend uses FULL-synchronous SQLite WAL for metadata and encoded objects up to 256 KiB. Larger objects use synchronized loose files. Prepared commit objects and records normally share the final membership/log transaction, reducing durability barriers; batches above the 16 MiB encoded buffer spill immutable content durably before final publication. Standalone blob writes and ref compare-and-swap remain independently durable operations. SQLite serializes writers, so this backend suits a single local authority and does not claim distributed write scalability.

The bundled SQLite dependency is 3.51.3, which includes the upstream [concurrent WAL-reset corruption fix](https://sqlite.org/releaselog/3_51_3.html). A linked-version regression test prevents reintroducing an older affected bundled runtime. Read-only library access uses `LocalStore::open_read_only`: it skips schema/data mutation and participates in live WAL coordination, rather than opening the database as an immutable snapshot. SQLite may still create or use WAL/SHM coordination sidecars when filesystem permissions allow.

Existing loose objects remain readable. This is a local storage-layout upgrade, not a change to object IDs or network encoding. Once small objects have been written to `inline_objects`, an older file-only Scion binary cannot read the complete repository. Plan a consistent backup before upgrade, and do not downgrade a live upgraded directory. Back up the database and any live WAL together with loose objects; the object directory alone is no longer sufficient.

Build the server with PostgreSQL support for shared transactional metadata:

```sh
cargo build -p scion-server --release --features postgres --locked
```

Configure `--database-url` with a PostgreSQL connection string. TLS connections use Rustls with the platform's trusted root certificates. Additional `--object-shard` connection strings select independent object-storage shards. Instances sharing the same repository authority must use compatible metadata and shard configuration. Keep the shard layout stable: changing a hash-to-shard mapping requires moving the existing objects or providing a migration layer.

The [cloud adapter](cloud-storage.md) stores immutable objects in S3, GCS, or Azure Blob Storage while retaining transactional metadata in SQLite, PostgreSQL, or Spanner. Shared authorities need the same shared metadata backend and cloud namespace. [Spanner deployment](spanner.md) covers the provisioned schema, identity, transaction limits, and official emulator conformance. The server's `spanner` feature includes cloud support; `--spanner-database` requires a configured cloud provider and cannot be combined with PostgreSQL metadata.

The storage interface separates immutable object operations from mutable metadata and reference CAS. A persistent backend must preserve durability, idempotence, and atomic reference updates; sharing only the object directory is not a substitute for shared transactional metadata.

### Change-log ordering

Changeset identity is independent of the change-log sequence. Concurrent PostgreSQL transactions can reserve sequence numbers and commit in a different order. Log writers use a shared advisory gate, and a log reader briefly takes the exclusive gate to establish a committed sequence frontier. This prevents a consumer from advancing its cursor past a transaction that has not committed yet.

Spanner publication increments a per-repository counter in the same transaction as membership, immutable records, and its log row. Strong readers therefore observe a committed sequence frontier. The counter is a coordination point; separate repositories have separate counters.

The repository engine does not acquire a repository-wide mutation mutex or move a ref when creating immutable commits. The durable backends still use transactions and storage-level coordination. Calling the entire deployment strictly lock-free would be inaccurate.

## Recovery

Back up authoritative metadata and objects as a consistent set. For LocalStore, stop writers and copy the complete store, or use a SQLite-aware backup procedure together with a consistent loose-object snapshot; copying an active database file without its WAL is insufficient. For shared databases, pair a consistent metadata snapshot with retained cloud or shard objects. Preserve policy configuration and the signing key separately according to the organization's secret-management process. Immutable objects may be copied incrementally, but references and derived-data mappings need a consistent restore point.

After restoring, start an isolated authority, check known refs and representative changesets, and materialize selected files to verify hashes and modes. Rebuild disposable node and regional caches from the authority or validated cache snapshots. Do not substitute cache data for missing authoritative history.

Uncommitted overlay work has its own backing directory; retain its database, local inode data, and related files together. Node/regional object caches are disposable, while dirty overlay bytes are user work. [Working-copy recovery](working-copies.md) documents ordinary checkout journals, mount synchronization, and safe overlay cleanup.

## Diagnostics

Record the service version and protocol version with incident reports. Monitor request errors and latency, cache hit/miss/eviction counts, upstream failures, object sizes, memory, disk usage, and backend latency. Inspect cache tiers independently: a warm regional cache can hide an unexpectedly cold node cache.

When a reference update conflicts, reload the current value and decide whether to rebase or merge before retrying. Do not turn an expected-value failure into an unconditional overwrite. When integrity verification fails, preserve the object ID and failing tier, then compare with the authority; replacing an invalid cache entry is different from repairing authoritative corruption.
