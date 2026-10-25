# Cloud object storage

`scion-store-cloud` stores immutable Scion objects in Amazon S3 and compatible services, Google Cloud Storage, or Azure Blob Storage. Transactional metadata remains in a separately selected `Storage` backend. A single authority can use SQLite metadata; multiple authorities should use shared PostgreSQL or [Spanner metadata](spanner.md) with the same cloud configuration.

The adapter uses Apache Arrow's maintained [`object_store`](https://docs.rs/object_store/0.14.2/object_store/) SDK. Version 0.14.2 supports Rust 1.85 and is compatible with this workspace's Rust 1.88 minimum. This is an object-storage adapter: it does not turn an object bucket into a transactional reference or change-log database.

## Configure the authority

Build the optional server features, or use an all-feature release bundle:

```sh
cargo build -p scion-server --release --features postgres,cloud --locked
```

Create the bucket/container before starting Scion. The service needs object read and conditional-create permissions within its configured prefix. Normal repository operation does not require bucket listing, object deletion, bucket creation, or bucket-policy administration.

### Amazon S3

```sh
export AWS_REGION=us-east-1
# Supply credentials through a workload role or the provider environment.
scion-server \
  --listen 127.0.0.1:7420 \
  --policy ./policy.json \
  --database-url "$SCION_DATABASE_URL" \
  --cloud-provider s3 \
  --cloud-bucket organization-source \
  --cloud-prefix production/scion
```

The capability signing key still comes from `SCION_CAPABILITY_KEY`; cloud credentials authorize the authority's storage access and have a separate purpose from client bearer tokens and object capabilities.

The [S3 builder](https://docs.rs/object_store/0.14.2/object_store/aws/struct.AmazonS3Builder.html) supports environment credentials (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and optional `AWS_SESSION_TOKEN`), web identity roles, ECS container credentials, and EC2 instance credentials. `--cloud-region` overrides the environment's region. Use `CloudStore::with_object_store` when an application needs a separately configured SDK credential provider. This adapter does not promise support for every credential format understood by the AWS command-line client.

For an S3-compatible service, provide its endpoint. Path-style requests are the default:

```sh
scion-server --policy ./policy.json --data ./metadata \
  --cloud-provider s3 --cloud-bucket source-objects \
  --cloud-region us-east-1 --cloud-endpoint https://objects.example.test
```

`--cloud-virtual-hosted-style` enables bucket-in-hostname addressing where the endpoint supports it. Plaintext private endpoints require the explicit `--cloud-allow-http` option and an explicit endpoint. The provider must implement atomic `If-None-Match: *` creation; a service that silently ignores that precondition is not a conforming backend.

### Google Cloud Storage

```sh
# Use the machine's application credentials or attached workload identity.
scion-server --policy ./policy.json --data ./metadata \
  --cloud-provider gcs --cloud-bucket organization-source \
  --cloud-prefix production/scion
```

The [GCS builder](https://docs.rs/object_store/0.14.2/object_store/gcp/struct.GoogleCloudStorageBuilder.html) discovers supported application credentials and metadata-service credentials. Explicit service-account file configuration is available through `GOOGLE_SERVICE_ACCOUNT` or `GOOGLE_SERVICE_ACCOUNT_PATH`; `GOOGLE_SERVICE_ACCOUNT_KEY` supplies JSON through the environment. The SDK also supports its application-credentials file formats and attached machine credentials. External identity formats should be checked against the selected SDK version or supplied through a custom SDK credential provider.

GCS writes use generation-match-zero preconditions. `--cloud-endpoint` can select a compatible GCS endpoint. `--cloud-anonymous-emulator` permits unsigned requests only when that explicit endpoint is on loopback; it cannot enable anonymous access to a remote GCS service.

### Azure Blob Storage

```sh
export AZURE_STORAGE_ACCOUNT_NAME=sourceaccount
# Supply managed identity, a service principal, or an SDK credential provider.
scion-server --policy ./policy.json --data ./metadata \
  --cloud-provider azure --cloud-bucket source-objects \
  --cloud-prefix production/scion
```

Here `--cloud-bucket` names the Azure container. `--cloud-account` overrides the account environment. The [Azure builder](https://docs.rs/object_store/0.14.2/object_store/azure/struct.MicrosoftAzureBuilder.html) supports account keys, service-principal configuration, managed identities, and explicit SDK credential providers. Common environment names include `AZURE_STORAGE_ACCOUNT_NAME`, `AZURE_STORAGE_ACCOUNT_KEY`, `AZURE_STORAGE_CLIENT_ID`, `AZURE_STORAGE_CLIENT_SECRET`, and `AZURE_STORAGE_TENANT_ID`.

Azure writes use create-if-absent preconditions. An explicit endpoint can target an emulator or a compatible deployment. Anonymous emulation has the same loopback restriction as GCS.

## Configuration reference

| Server option | Environment | Default |
|---|---|---|
| `--cloud-provider s3\|gcs\|azure` | `SCION_CLOUD_PROVIDER` | Cloud storage disabled |
| `--cloud-bucket NAME` | `SCION_CLOUD_BUCKET` | Required with a provider |
| `--cloud-prefix PREFIX` | `SCION_CLOUD_PREFIX` | `scion` |
| `--cloud-region REGION` | `SCION_CLOUD_REGION` | Provider environment |
| `--cloud-account ACCOUNT` | `SCION_CLOUD_ACCOUNT` | Provider environment |
| `--cloud-endpoint URL` | `SCION_CLOUD_ENDPOINT` | Provider endpoint |
| `--cloud-allow-http` | None | Disabled |
| `--cloud-virtual-hosted-style` | None | Disabled |
| `--cloud-anonymous-emulator` | None | Disabled; loopback GCS/Azure only |
| `--cloud-timeout-seconds N` | None | 60; valid range 1–300 |
| `--cloud-max-inflight N` | None | 16; valid range 1–1,024 |

Cloud objects and PostgreSQL `--object-shard` selection are alternative object backends; specifying both is rejected. PostgreSQL can still supply metadata through `--database-url` when cloud objects are enabled.

Prefixes must contain nonempty relative components made from ASCII letters, digits, dots, hyphens, or underscores. Empty, absolute, traversal, percent-encoded, and ambiguous prefixes are rejected. Endpoints may not contain URL user-info, query parameters, or fragments. Scion does not persist cloud credentials in repository metadata or working-copy state, and it does not propagate raw SDK error strings that could contain signed URLs or credential paths.

## Object layout and integrity

An object has the stable key:

```text
PREFIX/objects/v1/KIND/FIRST_TWO_HEX_DIGITS/FULL_OBJECT_ID
```

Kinds are `blob`, `tree`, `changeset`, and `derived`. Values are the same compressed binary envelopes used by the other Scion backends. Compression does not change object identity.

Writes create a key only if it is absent. If a concurrent or repeated write finds an existing key, the adapter reads and verifies the existing envelope. Valid alternative compression of the same content is accepted. A corrupt existing object produces an error and is never silently replaced by a normal write.

Reads check response metadata before allocating a full body, cap streamed bytes, verify the complete response length, and then validate envelope version, object kind, decoded length, and content hash. The format limits decoded objects to 256 MiB and encoded objects to 257 MiB. Embedding applications may lower the encoded limit with `CloudLimits`.

The cloud service's ETag and version headers support its protocol but do not replace Scion's content digest. Repository membership, path access, and signed object capabilities remain authority responsibilities; access to a cloud key is not itself client authorization.

## Runtime and failure behavior

The existing `Storage` API is synchronous. Each `CloudStore` owns a small dedicated I/O runtime, independent of the caller's runtime. Server storage work already runs in blocking workers. Embedded async applications should also call these methods through `spawn_blocking`.

Admission rejects excess remote operations before copying upload buffers. An operation deadline covers SDK retries and response-body consumption. The SDK is configured for up to three retries within that deadline. Timeout or abandonment aborts the submitted future, and dropping the final store uses nonblocking runtime shutdown, including when destruction occurs inside an async task.

A timed-out conditional upload may have completed remotely. Retrying the same immutable object is safe: it either creates the absent object or verifies the already-created value. Commit publication happens only after the engine has prepared its required objects; interrupted preparation can leave unreachable immutable objects without publishing a partial commit.

Choose concurrency together with object sizes and available memory. Sixteen concurrent maximum-sized bodies are materially different from sixteen small source files. Byte limits protect individual operations; deployment admission and memory sizing still matter.

## Backup and migration

Treat bucket/container, prefix, metadata authority, and object format as persistent configuration. Changing the prefix selects a different collection; it does not move existing objects. Copy and verify the immutable collection before changing routing, and restore metadata from a matching backup point.

Bucket versioning, provider replication, retention, encryption keys, recovery credentials, and lifecycle policies remain deployment configuration. Never configure lifecycle deletion to remove live Scion objects merely because they have not been downloaded recently. Cloud replication does not make a mismatched metadata backup consistent.

## Verification

```sh
cargo test -p scion-store-cloud --all-features --locked
go install github.com/minio/minio@RELEASE.2025-10-15T17-29-55Z
python scripts/test-s3.py --minio "$(go env GOPATH)/bin/minio"
```

The Rust suite exercises transactional metadata delegation, persistent reopen, concurrent duplicate writes, corrupt objects, alternate compression, size limits, bounded deadlines, admission recovery, and runtime destruction. HTTP contract tests exercise the actual S3, GCS, and Azure SDK clients against a local service that requires signatures and conditional-create headers.

The [Cloud storage workflow](../.github/workflows/cloud-storage.yml) also builds a pinned [official MinIO source release](https://github.com/minio/minio/releases/tag/RELEASE.2025-10-15T17-29-55Z) and runs the S3 service test with real HTTP, concurrency, and metadata restart. Its fixture creates a temporary loopback service and test credentials, then removes the test data. This is deployment conformance testing; production provider availability and workload latency require measurements in the intended account and region.
