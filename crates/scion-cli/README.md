# scion-cli

The `scn` command-line application and reusable Rust command runner for [Scion](https://github.com/wieslawsoltes/scion).

## Install and build

```sh
cargo install --path crates/scion-cli
# Linux virtual filesystem support:
cargo install --path crates/scion-cli --features fuse
```

When working from this workspace, use `cargo build --release -p scion-cli --features fuse`. The feature is optional; ordinary checkouts and overlay management work without a kernel mount.

## Working copies

```sh
scn --server http://127.0.0.1:7420 --repo example clone main work --sparse src --lazy
scn hydrate src/main.rs --path work
scn status work --stats
scn commit --path work --author 'Ada <ada@example.test>' -m 'Update source' --ref main
```

`--json` provides machine-readable output. `status --stats` includes cache and hashing counters; `status --refresh` discards saved fingerprints and verifies tracked contents. `watch` consumes filesystem notifications without retaining the working-copy lock, invalidates affected cached paths, and falls back to a full invalidation when notifications request a rescan.

The CLI also exposes history, blame, content search, diffs, merge, single-commit and stack rebase, revert, references, bisect, Git import/export, cache snapshots, and typed JSON RPC. Run `scn --help` and each subcommand's `--help` for the complete arguments.

## Writable virtual worktrees

```sh
scn overlay init main overlay --sparse src
mkdir mount
scn mount main mount --writable --overlay overlay
# In another terminal, edit files below mount, then:
scn overlay status overlay
scn overlay diff overlay
scn overlay commit overlay --author Ada -m 'Mounted changes' --ref main
fusermount3 -u mount
```

The mount command runs in the foreground. Its backing directory must be outside the mountpoint. Existing overlay revision and sparse selection are validated when reopened. `overlay info` shows the persisted revision and service addresses; `overlay prune` evicts clean hydrated contents according to `--cache-bytes`. Open and dirty files are retained. `overlay gc DIRECTORY` reclaims unlinked inodes and interrupted temporary data while the overlay is unmounted; kernel mounts also collect data as the last open/lookup reference closes. The default `mount` command remains read-only unless `--writable --overlay DIRECTORY` is supplied.

The authority supplies commit and reference semantics. Overlay commit preserves local data and its newly created revision if a reference compare-and-swap fails. Credentials are read from the invocation or environment and are never written into working-copy or overlay configuration.

## Embed the command runner

`Args` implements `clap::Parser`, and `run(&Args)` is the asynchronous reusable entry point used by the small `scn` binary. Applications requiring structured results should use `scion-client` directly; the command runner intentionally writes the CLI's formatted output.

See the workspace [command reference](https://github.com/wieslawsoltes/scion/blob/main/docs/commands.md), [getting started guide](https://github.com/wieslawsoltes/scion#quick-start), and operational documentation.

Licensed under MIT OR Apache-2.0.
