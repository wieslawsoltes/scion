# scion-store-spanner

Google Cloud Spanner metadata storage for [Scion](https://github.com/wieslawsoltes/scion), implemented with the official Google Rust SDK. Immutable objects are delegated to another `scion_store::Storage` backend; Spanner owns immutable metadata, reference compare-and-swap, membership, and the ordered publication log.

## Use

Provision a GoogleSQL database and apply the included `schema.sql` with an administrator. Runtime credentials need data access, not schema administration. Configure [Application Default Credentials](https://docs.cloud.google.com/docs/authentication/application-default-credentials), then construct the reusable backend:

```rust,no_run
use scion_store::{LocalStore, SharedStore, Storage};
use scion_store_spanner::{SpannerConfig, SpannerStore};
use std::sync::Arc;

# fn main() -> scion_types::Result<()> {
let objects: SharedStore = Arc::new(LocalStore::open("object-data")?);
let config = SpannerConfig::new("projects/my-project/instances/scion/databases/scion");
let store = SpannerStore::connect(objects, config)?;
assert!(store.compare_and_swap("refs/demo", "main", None, Some(b"initial"))?);
# Ok(())
# }
```

For a distributed authority, compose `CloudStore::connect(Arc::new(store), cloud_config)` so all immutable object I/O goes to S3, GCS, or Azure. The authority binary does this with its `spanner` feature and `--spanner-database` plus `--cloud-provider` options.

## Guarantees and bounds

Each publication commits its prepared records, managed changeset membership, deduplication record, counter, and log entry in one serializable transaction. A repository counter row orders publication commits so cursor consumers cannot miss a late lower sequence. Publication does not move refs; ref updates use independent atomic compare-and-swap. Aborted transactions retry through the official SDK.

The counter serializes publication within a repository. It is a correctness choice, not a claim of lock-free storage. Separate repositories have separate counters. Scans use strong reads and ordered limits. Metadata values are capped at 8 MiB, publications at 1,024 caller records and 32 MiB total, and scans at 100,000 rows and 64 MiB. Oversized operations return errors without partial publication.

The synchronous storage interface runs network I/O on a private bounded Tokio runtime. Async applications should call it from `spawn_blocking`. Deadlines cancel the operation future; an interrupted commit can have an uncertain remote outcome. Repeating immutable writes and publication is idempotent, and callers can inspect a ref before retrying a compare-and-swap.

The official `google-cloud-spanner` dependency is pinned to `0.34.4-preview` to retain Rust 1.88 compatibility. Its upstream API is a preview. This project does not claim equivalence to Anthropic's internal Spanner schema or deployment.

See [operations, schema, emulator tests, and limitations](https://github.com/wieslawsoltes/scion/blob/main/docs/spanner.md).

## License

MIT OR Apache-2.0.
