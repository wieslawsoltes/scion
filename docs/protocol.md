# Protocol and persisted formats

This document describes this repository's version 1 protocol. It is an independent contract; Anthropic has not published a Scion wire schema to reproduce. The Rust definitions in [`scion-protocol`](../crates/scion-protocol/src/lib.rs) and [`scion-types`](../crates/scion-types/src/lib.rs) are authoritative for field names and serialization.

## Endpoints

| Method and route | Available on | Request | Response |
|---|---|---|---|
| `GET /v1/info` | All tiers | None | Protocol, tier, and implementation version |
| `POST /v1/repos/{repo}/rpc` | Authority | JSON `RpcRequest` | JSON `RpcReply` |
| `POST /v1/repos/{repo}/blobs` | Authority | Raw uncompressed bytes | `RpcReply` containing blob ID |
| `POST /v1/repos/{repo}/objects` | All tiers | JSON `ObjectRequest` | Verified binary object envelope |
| `GET /health` | All tiers | None | Liveness |
| `GET /ready` | All tiers | None | Readiness; authority checks metadata access |
| `GET /metrics` | All tiers | None | Text operational metrics |

The RPC endpoint uses `Content-Type: application/json`. Authenticated control and upload requests carry `Authorization: Bearer TOKEN`. Object requests carry a signed capability rather than a bearer token. Cache tiers serve trees and blobs; derived objects remain internal to the authority.

Example:

```sh
curl --fail-with-body \
  -H 'Content-Type: application/json' \
  -H "Authorization: Bearer $SCION_TOKEN" \
  --data '{"method":"list_refs"}' \
  "$SCION_SERVER/v1/repos/$SCION_REPO/rpc"
```

## Control replies

A successful reply has a `result` and no error:

```json
{"result":{"main":"FULL_64_HEX_CHANGESET_ID"},"error":null}
```

An unsuccessful reply has an error code and message:

```json
{"result":null,"error":{"code":"conflict","message":"reference conflict"}}
```

| Code | HTTP status | Meaning |
|---|---|---|
| `invalid` | 400 | Invalid input or format |
| `permission` | 403 | Authentication or authorization failed |
| `not_found` | 404 | Object, revision, or path not found in this repository |
| `conflict` | 409 | Conflicting operation |
| `limit` | 429 | Admission, rate, or operation limit |
| `internal` | 500 | Authority-side failure; internal details are not returned |
| `upstream` | 502 | Cache could not complete its configured upstream request |

Framework-level failures, such as a malformed JSON body or an oversized HTTP request, may be returned before dispatch and need not use `RpcReply`. Clients must handle non-RPC error bodies. A reference CAS mismatch is a successful RPC with `result: false`, allowing callers to distinguish a lost race from a transport failure.

## Control methods

Each request is a JSON object tagged by `method` in snake case. IDs below are full object IDs, paths are normalized repository-relative strings, and timestamps are Unix milliseconds.

| Method | Fields | Result |
|---|---|---|
| `put_blob` | `data` (base64) | Blob ID |
| `commit` | `changeset` | Changeset ID |
| `get_changeset` | `id` | Changeset |
| `resolve` | `revision` (ID or ref name) | Changeset ID |
| `get_ref` | `name` | ID or null |
| `set_ref` | `name`, `expected`, `target` | Boolean CAS result |
| `list_refs` | None | Map of names to IDs |
| `root_tree` | `commit` | Root tree ID |
| `tree_at` | `commit`, `path` | Directory tree ID |
| `list_files` | `commit`, `prefix` | Path-to-file-entry map |
| `list_files_page` | `commit`, `prefix`, `after`, `limit` | `entries` map and `next` cursor |
| `read_file` | `commit`, `path` | Base64 bytes |
| `diff` | `base`, `target` | File differences and optional text patches |
| `log` | `revision`, `limit` | Ordered ID/changeset pairs |
| `history` | `revision`, `path`, `limit` | Changeset IDs touching the path |
| `blame` | `revision`, `path` | Line attribution records |
| `search` | `revision`, `prefix`, `query`, `limit` | Matching repository paths |
| `copies` | `revision` | Destination-path-to-copy-source map |
| `is_ancestor` | `ancestor`, `descendant` | Boolean |
| `merge_bases` | `a`, `b` | All maximal common ancestor IDs |
| `nth_parent` | `revision`, `distance` | First-parent ancestor ID or null |
| `merge` | `ours`, `theirs`, `author`, `message`, `timestamp_ms` | Merge result |
| `rebase` | `revision`, `onto` | Merge result |
| `rebase_stack` | `tip`, `base`, `onto` | Rebased tip, old-to-new mapping, and stopping conflicts |
| `revert` | `revision`, `onto`, `mainline`, `author`, `message`, `timestamp_ms` | Merge result |
| `bisect` | `good`, `bad`, `skip` | Candidate ID or null |
| `object_access` | `commit`, `path`, `kind`, `id` | Signed object ticket |
| `object_access_batch` | `objects` (array of witnesses) | Array of signed tickets |
| `change_log` | `after`, `limit` | Sequence/ID entries |

`set_ref.expected = null` means create only if absent. `target = null` means deletion, still conditioned on the expected old value. The expected value is never implicitly ignored.

A merge result contains `changeset` and `conflicts`. Successful operations contain an ID and an empty conflict list. Unresolved operations contain `changeset: null` and path-scoped conflict records with base/ours/theirs entries. This is a semantic result, not necessarily an HTTP error. An API caller must inspect it before treating the operation as completed.

`rebase` replays one changeset. `rebase_stack` replays the first-parent interval `(base, tip]` onto `onto`. Its result contains `rewritten` (an old-to-new ID map), `tip` (the last successfully replayed revision), and `conflicts`. Replay stops at the first conflict, while completed immutable rewrites remain available. Both operations also exist in the reusable engine API. `bisect` selects a candidate; it does not execute a test process. `search` searches paths, not arbitrary file contents.

`list_files_page` returns lexically ordered, authorized paths. Start with `after: null`, then pass the response's `next` value as the next request's exclusive `after` cursor. A null `next` marks completion. The authority advances over inaccessible entries internally and returns only a visible path as a continuation cursor, so pagination does not reveal hidden filenames. Keep the same immutable commit and prefix throughout the scan. Prefer this method when a full manifest could exceed the control-reply bound.

`history` also accepts a directory prefix, including a directory that has been deleted. Directory history requires read access throughout that subtree because its result can reveal descendant activity.

## Changeset representation

```json
{
  "parents": ["FULL_64_HEX_PARENT_ID"],
  "author": "Ada <ada@example.test>",
  "timestamp_ms": 0,
  "message": "Update greeting",
  "changes": {
    "hello.txt": {
      "op": "put",
      "entry": {
        "id": "FULL_64_HEX_BLOB_ID",
        "size": 14,
        "kind": "regular"
      }
    },
    "obsolete.txt": { "op": "delete" }
  }
}
```

Optional `committer` and `metadata` fields are omitted from canonical encoding when empty according to the Rust serialization definitions. A put can include `copy_from: {"changeset": "ID", "path": "source/path"}`. File kinds are `regular`, `executable`, `symlink`, and `submodule`.

The first parent defines the delta base, including for merge commits. Parent order participates in identity. Duplicate parents and live file/directory path collisions are rejected. The author and message are data supplied by the caller; authorization is determined by the authenticated principal, not by the author text.

## Object capabilities

First ask the authority for a ticket using an authorized witness:

```json
{
  "method": "object_access",
  "commit": "FULL_CHANGESET_ID",
  "path": "hello.txt",
  "kind": "blob",
  "id": "FULL_BLOB_ID"
}
```

The authority verifies that the object is the file or directory at that revision and path. A ticket contains `repo`, `commit`, `path`, `kind`, `id`, `expires_ms`, and `signature`. Send it unchanged to the selected object tier:

```json
{"ticket":{"repo":"demo","commit":"ID","path":"hello.txt","kind":"blob","id":"ID","expires_ms":0,"signature":"MAC"}}
```

The example placeholders are descriptive; the authority must issue a real ticket with valid IDs, expiry, and signature. Signatures use keyed BLAKE3 over the canonical tuple of scope fields. Expiry is at most five minutes in the future. All tiers reject invalid signatures, incorrect repository scope, and expired tickets before serving bytes.

Trees reveal child names and therefore require read access throughout their subtree. A partially authorized principal can use filtered file-list/diff/search APIs instead. Complete changesets, root trees, full logs, and graph queries require full repository read access. Reference mutation additionally requires repository `admin` permission.

## Binary object envelope

Responses use `application/vnd.scion.object`. Byte offsets are fixed:

| Offset | Length | Field |
|---|---|---|
| 0 | 8 | ASCII `SCNOBJ01` |
| 8 | 1 | Kind: blob=1, tree=2, changeset=3, derived=4 |
| 9 | 8 | Uncompressed length, unsigned little-endian |
| 17 | 32 | Raw content ID bytes |
| 49 | Remaining | Zstandard frame |

The content ID hashes `scion\0v1\0`, the UTF-8 kind name, a NUL separator, and the uncompressed payload. The compressor currently uses Zstandard level 3. Consumers must verify the envelope's kind, expected ID, decoded length, and recomputed content hash before trusting the payload.

Object HTTP responses use `Cache-Control: private, no-store` because ordinary HTTP caches do not validate Scion capabilities. The dedicated Scion cache stores encoded objects independently and performs ticket validation for every request.

## Bounds

| Boundary | Current limit |
|---|---|
| Decoded object | 256 MiB |
| Encoded envelope accepted by decoder | 257 MiB |
| Control RPC request/client reply | 16 MiB |
| Object capability batch | 256 witnesses |
| Changeset parents | 64 unique ordered IDs |
| Changeset author | 4,096 bytes |
| Changeset message | 1 MiB |
| Repository path | 4,096 bytes; 255 bytes per component |
| Commit path nesting | 256 components |
| Service paged-query limit | Clamped to 1–10,000 |

Large file uploads use the raw blob endpoint rather than base64 inside a control RPC. Queries returning many entries can exceed the client reply bound; use sparse prefixes and bounded requests. Limits are implementation choices and may be configurable or versioned in a future release.

## Compatibility and retries

`/v1/info` advertises protocol version `1` and a tier of `server`, `cache`, or `daemon`. A daemon requires a cache upstream; a cache requires an authority upstream. Startup rejects an incompatible protocol or topology. An outage does not enable bypassing the configured tier.

Immutable object fetches and identical object uploads are safe to retry. Retrying an identical changeset produces the same content ID and a deduplicated log record. For mutable refs, always retry with an explicit expected value and inspect the result. There is no push operation or pack negotiation in this protocol.
