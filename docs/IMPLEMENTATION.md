# Implementation plan

Implement the public Scion model in independently reusable Rust crates:

- Canonical immutable first-parent changesets and compressed content-addressed objects.
- Append-only changeset log, independent compare-and-swap refs, local and shared storage backends.
- Persistent directory trees; a generic recursive derived-data framework; graph, blame, history, copy and search indexes.
- Server APIs for commit, diff, ancestry, merge bases, merge, rebase, revert, bisect and file queries.
- A thin sparse client, bounded caches, on-demand materialization, filesystem change monitoring and Git ingestion.
- Identical immutable-object RPCs through authoritative server, regional cache and node daemon, with strict tiering and snapshots.
- Repository/directory authorization, observability, resource bounds and hermetic integration tests.
- Cross-platform builds, release binaries, publishable library crates, examples and operational documentation.

The public article is an architectural description, not a complete protocol specification. Concrete formats and algorithms will be documented; private upstream compatibility and performance claims will not be asserted without evidence.
