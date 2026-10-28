# HTTP policy TLS fixture

`http-policy-cert.der` is a self-signed, test-only certificate for `allowed.test`.
`http-policy-key.der` is its public repository test fixture private key in PKCS#8
DER form. Neither is a production credential or a trusted runtime certificate.
Only the TLS unit test explicitly adds the certificate to its private root store.
The production transport uses the WebPKI root bundle.

The certificate is valid from 2026-10-28 through 2126-10-04. Regenerate the pair
with OpenSSL if the test hostname or validity period changes. Hostname mismatch
and untrusted-root cases must continue to fail.
