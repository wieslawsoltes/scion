# Contributing to Scion

Scion is an independent Rust implementation of the publicly described source-control architecture. Changes should make the implementation easier to reuse, verify, or operate without inventing compatibility with unpublished upstream internals.

## Development environment

Install the current stable Rust toolchain with `rustfmt` and `clippy`, Python 3.11 or later for release tooling, and Git for migration tests. The supported minimum Rust version is declared in `Cargo.toml`; CI checks it separately from the stable development toolchain. A C compiler is needed for bundled SQLite and compression dependencies.

```sh
git clone https://github.com/wieslawsoltes/scion.git
cd scion
cargo build --workspace --locked
cargo test --workspace --all-features --locked
```

Before opening a pull request:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo doc --workspace --all-features --no-deps --locked
python scripts/release.py check-metadata
cargo package --workspace --all-features --locked
```

Workspace packaging uses a recent stable Cargo that can verify unpublished workspace dependencies together. The compiler MSRV is a separate compatibility target.

## Change boundaries

Place reusable behavior in the smallest appropriate crate. Keep `scion-cli` focused on command parsing and output; put protocols in `scion-protocol`, repository semantics in `scion-engine`, and persistent storage behind `scion-store` interfaces. Libraries must remain usable without spawning the CLI.

Document changes to canonical encodings and persistence schemas as format changes. Do not casually change struct field order or serialization attributes in persisted types. Preserve byte-for-byte canonical-encoding tests, or introduce an explicit new version with a migration plan.

Do not add a full-repository scan to a mutation path without an explicit complexity analysis. Include the number of changed paths, directory depth, history length, and data read/written when explaining performance changes.

## What to test

Tests should protect externally observable invariants:

- A failed reference compare-and-swap never overwrites the winning value.
- A successfully acknowledged changeset remains readable after restarting storage.
- Merge ancestry includes all parents.
- A cache hit returns exactly the same object bytes as the authority.
- Directory permissions remain effective through caches and metadata APIs.
- Unmaterialized sparse files are not recorded as deletions.
- Working-copy updates preserve dirty and untracked user files unless explicit replacement was requested.
- Git ingestion is resumable and preserves parents, content, and supported file modes.

Use deterministic fixtures for graph and merge behavior. Use temporary directories for persistence tests. Add integration tests when a bug spans a service boundary. Keep benchmarks separate from correctness tests, and report the workload and machine alongside timing results.

## Pull requests

Explain the problem, the resulting behavior, and how it was verified. Identify public API, format, MSRV, and deployment changes. Update [the public-design matrix](docs/public-design.md) when a change completes or narrows a published requirement.

Avoid unsupported claims such as production validation, transparent virtual filesystem support, or compatibility with Anthropic's private service. Mark the specific tested behavior instead.

## Licensing

Contributions are accepted under `MIT OR Apache-2.0`, matching the workspace. Include attribution and compatible license notices for third-party code. Architectural references are listed in [the public-design document](docs/public-design.md).
