# Building and releasing

## Deliverables

All thirteen product workspace crates are independently packageable. The workspace uses one release version and versioned dependencies between crates. There are four executable deliverables:

| Executable | Package | Purpose |
|---|---|---|
| `scn` | `scion-cli` | User and automation command interface |
| `scion-server` | `scion-server` | Authoritative repository API |
| `scion-cache` | `scion-cache` | Regional immutable-object cache |
| `scion-daemon` | `scion-cache` | Node cache and optional Unix-socket endpoint |

Build all executables:

```sh
cargo build --workspace --bins --release --all-features --locked
```

Create and verify all crate archives, including unpublished interdependent crates:

```sh
python scripts/release.py check-metadata
cargo package --workspace --all-features --locked
```

Use current stable Cargo for workspace packaging. The MSRV check verifies source compilation and is independent of release-tooling requirements.

## Continuous integration

[CI](../.github/workflows/ci.yml) runs on pull requests, pushes to `main`, and manual dispatch. It includes:

1. Rust formatting, Clippy with warnings denied, rustdoc with warnings denied, and documentation tests.
2. Workspace builds and tests on Linux, macOS, and Windows.
3. A build using the minimum supported Rust compiler.
4. Metadata checks and full `cargo package` verification for every crate.
5. Dependency license, source, and RustSec advisory checks with `cargo-deny`.
6. PostgreSQL storage conformance and a built/running server → regional cache → node daemon container topology.

Additional workflows run real Linux FUSE I/O and publication, provider SDK contracts and MinIO, official Spanner emulator transactions, three sanitizer fuzz targets, and a correctness-checked durable Git comparison. Path filters select the relevant integration workflows for a PR; fuzzing and the full Git comparison also have scheduled runs. [Testing](testing.md) links the workflow definitions and completed evidence.

[Dependabot](../.github/dependabot.yml) opens weekly Cargo, GitHub Actions, and container-base updates. Dependency updates must preserve MSRV or deliberately raise the workspace requirement and update the CI job.

## Versioned release workflow

The [release workflow](../.github/workflows/release.yml) runs for `v*.*.*` tags. It can also be rerun through workflow dispatch with an existing version tag. It verifies that the checked-out commit is exactly the tagged commit and that every crate version matches the tag.

It then performs validation and builds native binary bundles for:

| Target | Runner |
|---|---|
| `x86_64-unknown-linux-gnu` | Ubuntu 24.04 x64 |
| `aarch64-unknown-linux-gnu` | Ubuntu 24.04 ARM64 |
| `aarch64-apple-darwin` | macOS 15 ARM64 |
| `x86_64-apple-darwin` | macOS 15 Intel |
| `x86_64-pc-windows-msvc` | Windows Server 2025 |

Each bundle contains all four executables, licenses, user documentation, and `RELEASE.json` identifying the source commit, target, and compiler. Unix bundles preserve executable permissions. Windows bundles use ZIP. Archive timestamps are normalized to the source commit time; this does not imply that compiler outputs are bit-for-bit reproducible across machines.

Release binaries are built with all workspace features, including PostgreSQL, S3/GCS/Azure objects, Spanner metadata, and Linux read-only/writable FUSE mounts. Default source builds can choose a smaller feature set. FUSE still requires host filesystem support and appropriate mount permissions at runtime.

The GitHub release contains the bundles, a source archive, all `.crate` archives, and `SHA256SUMS`. The release becomes visible only after all binary builds succeed and the assets have been uploaded. Release reruns can replace assets for the same tag; do not move an already published tag to another commit.

To verify a downloaded file on Linux:

```sh
sha256sum --check SHA256SUMS --ignore-missing
```

On macOS use `shasum -a 256`; on PowerShell use `Get-FileHash -Algorithm SHA256`. Compare the digest against the same filename in `SHA256SUMS`.

## Publishing to crates.io

Packaging and publishing are separate steps. CI verifies packages without registry credentials. Actual crates.io publication runs only when the repository variable `PUBLISH_CRATES` equals `true` and the `crates-io` environment supplies `CARGO_REGISTRY_TOKEN` with publish permissions for the selected crate names. Configure ownership of the crate names before enabling publication.

The publishing script derives dependency order from `cargo metadata`; it does not maintain a second hand-written dependency list. It waits for each uploaded version to become visible before continuing. A rerun skips an already published version only when its registry checksum matches the locally packaged archive. A differing checksum stops publication.

Preview the order without publishing:

```sh
python scripts/release.py order
python scripts/release.py publish
```

For an authorized local publication from a clean tagged checkout:

```sh
export SCION_RELEASE_TAG=v0.1.0
python scripts/release.py check-tag
cargo package --workspace --all-features --locked
# Supply CARGO_REGISTRY_TOKEN through your credential manager or CI secret store.
python scripts/release.py publish --execute
```

Do not paste tokens into tracked files or command arguments. Publishing a version is an external registry operation; the workflow definition does not by itself establish that the crates have already been published.

## Local-store upgrade compatibility

This version stores small immutable objects in SQLite as well as larger loose files. Existing loose-object stores remain readable, but older file-only binaries cannot read newly inlined objects. Back up the complete authoritative store, including the database/WAL, before changing binaries; do not downgrade an upgraded live directory. Object IDs and network envelopes are unchanged. The bundled SQLite runtime is 3.51.3 to include the upstream [WAL-reset corruption fix](https://sqlite.org/releaselog/3_51_3.html); the workspace checks the linked version. See [persistence and recovery](operations.md#persistence-and-scaling).

## Preparing a version

Update `workspace.package.version` and all internal dependency version requirements together. Update the lockfile, compatibility notes, examples, and any migration instructions. Run CI, review the package contents, and create the corresponding tag on the validated commit. For a format change, include explicit reader/writer compatibility and rollback behavior in the release notes.
