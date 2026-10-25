# Identity federation and policy reload

`scion-auth` is a reusable library for static bearer credentials, signed JWT
identities from configured OIDC providers, repository permissions, and directory
rules. The authority authenticates control requests and uploads before reading
their bodies, then checks the active policy again before accessing a repository.

## Federated identities

Configure the provider that issues tokens for your Scion deployment. The token
must have a signature, a key identifier (`kid`), the configured `iss` and `aud`,
a nonempty `sub`, and an unexpired `exp`. When present, `nbf` is also checked.
Scion accepts compact signed JWTs; opaque OAuth access tokens and encrypted JWTs
are not accepted. Your identity provider must issue a JWT intended for this API.

```json
{
  "issuers": [
    {
      "issuer": "https://identity.example.com/tenant",
      "audience": "scion-production",
      "algorithms": ["RS256"],
      "cache_seconds": 300,
      "leeway_seconds": 30
    }
  ],
  "principals": [
    {
      "name": "alice",
      "identities": [
        {
          "issuer": "https://identity.example.com/tenant",
          "subject": "provider-assigned-stable-subject"
        }
      ],
      "repositories": {
        "product": {
          "read": true,
          "write": true,
          "admin": false,
          "rules": [
            {"prefix": "restricted", "read": false, "write": false},
            {"prefix": "restricted/public", "read": true, "write": false}
          ]
        }
      }
    }
  ]
}
```

The exact issuer and subject pair selects a principal. Email, display name,
groups, and other token claims never create permissions implicitly. This follows
the stable identifier guidance in [OpenID Connect Core section 5.7][oidc]. A
binding may belong to only one principal. Each principal must have at least one
identity binding or a static token hash. A principal can have both, supporting
an explicit transition between credential types.

Without `jwks_url`, Scion retrieves
`ISSUER/.well-known/openid-configuration`, requires its `issuer` to match exactly,
and uses its `jwks_uri`. An administrator can instead configure `jwks_url`
explicitly. Issuers and key endpoints require HTTPS. URLs in a token's `jku`,
`x5u`, or embedded key fields do not select a trust source. Redirects are disabled.
An unverified `iss` claim only selects an already configured provider.

The allowed algorithms are RS256/384/512, PS256/384/512, ES256/384, and EdDSA.
The default is RS256. Symmetric HMAC algorithms and unsigned tokens are rejected.
Each signing key is bound to one algorithm. When a JWK omits `alg`, exactly one
configured algorithm must match its key family; otherwise that key is unusable.
Unknown critical JOSE extensions are rejected. Algorithm and audience checks
follow the constraints described in [RFC 8725][jwt-bcp].

Scion verifies bearer identity; it does not run an interactive OAuth login flow,
provision accounts, register provider applications, or refresh a user's token.
Obtain the API token using the provider's supported flow and supply it to the
client through `SCION_TOKEN` or the reusable client's token argument. The client
does not persist the bearer token in a checkout or writable overlay.

## Static credentials

Existing policies remain valid without an `issuers` field. Generate a random
credential, then use the server's hash command:

```sh
export SCION_TOKEN="$(openssl rand -hex 32)"
scion-server --hash-token
```

Put the printed hash in a principal's `token_hash` field. Keep the original
credential in the client's secret configuration. Token hashes accept upper or
lowercase hexadecimal; equivalent hashes cannot identify two principals.
Duplicate Authorization headers, empty bearer values, and bearer values with
whitespace are rejected.

## Atomic policy replacement

```sh
scion-server --data ./scion-data --policy ./policy.json \
  --policy-reload-seconds 10
```

Set `SCION_CAPABILITY_KEY` before launching an authenticated authority. The
default reload interval is ten seconds. Set `--policy-reload-seconds 0` to disable
polling. Use an atomic file replacement when updating the JSON policy.

The server reads at most 8 MiB, parses the entire policy, checks all bindings and
rules, and constructs a replacement before making it visible. Invalid JSON,
duplicate identities, missing provider definitions, invalid URLs, and filesystem
errors leave the last valid policy active and produce a diagnostic. Identical
policies retain their signing-key caches. A valid empty principal list explicitly
revokes every authenticated principal; initial production startup still requires
a nonempty policy.

Authentication returns an opaque `Authenticated` proof containing the principal,
token expiry, and policy fingerprint, without retaining the bearer token. After
request buffering, authorization rejects a proof from a different policy version
or an expired token. Requests already executing an authorized repository operation
finish under the permissions they acquired. Hot reload does not interrupt a
transaction already in progress.

Embedding applications can call `ServerState::policy_store()` and use
`PolicyStore::replace` or `reload_file` directly. Policy replacement is local to
each authority process. Deployments with several authorities must distribute the
same policy and account for their reload interval.

## Signing-key rotation and bounds

| Resource or behavior | Limit |
|---|---|
| Bearer value | 16 KiB |
| Discovery or JWKS document | 1 MiB, checked while streaming |
| Keys in one JWKS | 256 |
| Key identifier | 1–256 bytes |
| Subject | 1–512 bytes |
| Provider HTTP request | 5 seconds |
| Key-cache lifetime | 30–3,600 seconds; default 300 |
| Refresh attempt spacing | At least 30 seconds per provider |
| Clock leeway | 0–120 seconds; default 30 |

Concurrent requests share one refresh per provider. An unknown key ID can cause
a refresh after the throttle interval; it cannot cause a network request for
every token. Provider failures are throttled as well. Fresh cached keys continue
to verify known keys until their configured lifetime ends. Expired key caches
fail closed when refresh fails. Providers should overlap old and new keys long
enough for their issued tokens and consumer cache policies.

Admission is acquired before asynchronous authentication, bounding requests that
wait for a provider. Repository and directory permissions are then enforced by
the same authority rules used for static credentials. Key caches and token
verification are process-local; there is no shared token session database.

For a local test provider only, `allow_http_loopback: true` permits HTTP URLs
whose host is `localhost` or a loopback IP address. HTTP to other hosts remains
invalid. This exception must be configured explicitly.

## Object capabilities and revocation

Object access uses signed capabilities scoped to the repository, revision, path,
object kind, and content ID. They expire after five minutes. A policy reload
prevents new capabilities from being minted under revoked permissions, but
already issued capabilities remain valid until expiry. Previously downloaded
source cannot be revoked. Capability-key rotation and cache trust topology are
separate operational concerns described in [operations](operations.md).

## Verification

```sh
cargo test -p scion-auth --locked
cargo test -p scion-server --test system --locked
```

The identity suite runs against a temporary loopback discovery/JWKS service and
explicit public test keys. It checks signed identity mapping, wrong audience and
issuer, missing claims, expiry, future validity, signature mismatch, algorithm
confusion, duplicate key IDs, bounded documents, key rotation, coalescing,
provider outages, and policy replacement. Authority integration checks credential
revocation and proves an unauthorized 100 MiB upload is rejected before any body
bytes are sent. Tests require no production identity-provider account.

[oidc]: https://openid.net/specs/openid-connect-core-1_0.html#ClaimStability
[jwt-bcp]: https://www.rfc-editor.org/rfc/rfc8725.html
