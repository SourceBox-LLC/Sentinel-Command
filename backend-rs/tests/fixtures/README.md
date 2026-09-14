# Test fixtures

`test_signing_key.pem` and `test_jwks.json` are a throwaway RSA-2048
keypair generated for `tests/clerk_verifier.rs`. They sign tokens for a
JWKS server the test starts on localhost.

This key protects nothing and is deliberately committed: the test needs
a stable keypair, and generating one per run would cost ~100ms and make
a failure harder to reproduce. It is not accepted by any deployment —
the issuer it is served under (`127.0.0.1:<random port>`) is not a Clerk
instance, and production derives its issuer from `CLERK_PUBLISHABLE_KEY`.
