# Scion

Rust source control built from the public architectural description of [Scion](https://www.anthropic.com/research/redesigning-source-control).

This is an independent implementation, unaffiliated with Anthropic. The upstream service is not open source and does not publish a wire protocol. Compatibility targets the publicly described behavior, not its private implementation.

Implementation is being developed in a pull request with reusable crates, a server and thin client, content-addressed storage, recursive derived data, sparse working copies, tiered caches, Git migration, documentation, and build/release automation.

Licensed under MIT OR Apache-2.0.
