# Security policy

## Reporting a vulnerability

Use [GitHub private vulnerability reporting](https://github.com/wieslawsoltes/scion/security/advisories/new) when it is enabled. If private reporting is unavailable, open an issue asking the maintainer for a private contact channel without including exploit details, credentials, or private repository contents.

Include the affected revision, deployment topology, reproducible steps, expected authorization boundary, and observed result. Reports involving content corruption should include a minimal synthetic repository rather than confidential source files.

## Supported versions

Security fixes target the latest released version and the default development branch. This project does not currently promise a long-term-support release schedule.

## Security boundaries

Scion distinguishes immutable object identity from permission to read an object. An object hash is an integrity identifier. It is not an access credential. The authority issues signed, short-lived capabilities after checking the repository, revision, path, and object relationship; cache tiers verify those capabilities before serving bytes.

The capability signing key is shared by services in one trust domain. A holder of that key can mint capabilities, so cache operators are trusted infrastructure operators. Rotate the key when removing a service from that trust domain. Cached bytes can outlive a capability, but an expired capability must not authorize a read. An already issued capability can remain valid until its expiry, even after an access-policy change.

The supplied services expose HTTP endpoints. Terminate TLS at a trusted reverse proxy or service mesh before exposing them across untrusted networks. Use separate bearer credentials per principal and keep configuration containing credentials outside the repository. Restrict metrics endpoints to the operational network when their data could reveal repository activity.

Working copies reject traversal and reserved metadata paths. Treat symbolic links, directory/file replacements, and untracked-file preservation as security-sensitive behavior. A source-control client must not write outside the selected checkout root.

## Operational responsibilities

Authoritative metadata and object storage require backups and access controls. Node and regional caches are disposable copies and are not authoritative backups. Exercise restoration into a separate environment and verify object integrity before accepting writes there.

Do not run an unauthenticated development configuration on an externally reachable interface. The example local deployment is for functional evaluation; production configuration depends on your identity system, storage backend, TLS termination, and network topology. See [operations](docs/operations.md).
