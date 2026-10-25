# scion-store-cloud

Verified immutable Scion objects on Amazon S3 and compatible services, Google Cloud Storage, and Azure Blob Storage.

`CloudStore` implements the reusable `scion_store::Storage` interface. It sends objects to an existing cloud bucket/container and delegates references, immutable records, commit publication, and change logs to an independent transactional metadata backend. It integrates with `LocalStore` for one authority and `PostgresStore` for a shared authority.

```rust,no_run
use scion_store::{LocalStore, Storage};
use scion_store_cloud::{CloudConfig, CloudProvider, CloudStore};
use scion_types::ObjectKind;
use std::sync::Arc;

# fn main() -> scion_types::Result<()> {
let metadata = Arc::new(LocalStore::open("scion-metadata")?);
let mut config = CloudConfig::new(CloudProvider::S3, "my-source-objects");
config.region = Some("us-east-1".into());
config.prefix = "production/scion".into();
let storage = CloudStore::connect(metadata, config)?;
let id = storage.put_object(ObjectKind::Blob, b"source bytes")?;
assert_eq!(storage.get_object(ObjectKind::Blob, id)?, b"source bytes");
# Ok(())
# }
```

The default features enable all three providers. Select `aws`, `gcp`, or `azure` independently when a smaller dependency surface is useful. Provider credentials come from the maintained `object_store` SDK's provider chains or from a custom `Arc<dyn object_store::ObjectStore>` supplied to `CloudStore::with_object_store`.

All writes use atomic create-if-absent preconditions. Duplicate writes verify the existing object instead of overwriting it. Reads enforce encoded and decoded byte limits, envelope kind, declared length, and content digest. SDK failures are mapped to categories without propagating raw credential-bearing error strings.

The synchronous API uses its own bounded async I/O runtime, operation deadlines, and cancellation guards. Async applications should call storage operations through `tokio::task::spawn_blocking`. Dropping the final store inside async code does not synchronously destroy a Tokio runtime.

See [cloud deployment and credentials](https://github.com/wieslawsoltes/scion/blob/main/docs/cloud-storage.md), [workspace architecture](https://github.com/wieslawsoltes/scion/blob/main/docs/architecture.md), and the generated Rust API documentation.

Licensed under MIT OR Apache-2.0.
