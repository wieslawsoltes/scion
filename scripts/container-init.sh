#!/bin/sh
# Generate a local evaluation policy without persisting the plaintext token.
set -eu
umask 077
token_hash="$(scion-server --hash-token)"
printf '{"principals":[{"name":"operator","token_hash":"%s","repositories":{"demo":{"read":true,"write":true,"admin":true}}}]}\n' "$token_hash" > /config/policy.json
printf 'Configured the authenticated demo repository policy.\n'
