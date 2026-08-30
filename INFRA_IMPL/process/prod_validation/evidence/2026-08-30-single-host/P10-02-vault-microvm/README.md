# P10-02 real Vault microVM evidence

Result: PASS on 2026-08-30.

`result.json` is the machine-readable summary. The node logs cover initial
seal, controlled rewrap, current-only restart, sealed-Vault rejection, and
post-unseal recovery. `vault-audit.jsonl` contains Vault's HMAC-protected audit
records. `redaction-scan.txt` proves the unique sentinel was absent from all
five node logs and the audit artifact. `SHA256SUMS` binds every evidence file
except itself.

No Vault token, SecretID, unseal key, root token, HMAC value, or sentinel value
is intentionally included. The three request IDs in `result.json` safely
correlate pinned-old HMAC, rotation, and pinned-new HMAC operations.

The final run rotated Transit version 3 to 4. Earlier attempts exposed and
fixed private-CA trust, real base64 decoding, HMAC key creation, Alpine PID 1,
non-TTY unseal, WSL permission, and cluster-replay readiness issues. See the
[complete runbook](../../../../VAULT_TRANSIT_MICROVM_VALIDATION.md).
