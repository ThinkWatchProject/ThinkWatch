# Secret rotation runbook

This file documents the procedures for rotating the two
load-bearing secrets in a ThinkWatch deployment:

| Secret | Affects | Frequency target |
|---|---|---|
| `JWT_SECRET` | Issuing + verifying access / refresh JWTs | Annual + on suspected compromise |
| `ENCRYPTION_KEY` | At-rest envelope (provider headers, MCP OAuth client secret, AWS keys, TOTP secret) | Per security policy — yearly is normal |

Both can be rotated without dropping in-flight requests, but each has
a specific sequence. Read the whole section for the secret you are
about to rotate **before** you run any command.

---

## JWT_SECRET

### What changes if you rotate it wrong

- Every existing access token issued by the old secret stops verifying
  immediately. Logged-in users get `401 Unauthorized` on the next call
  until they refresh.
- Every refresh token issued by the old secret stops verifying too.
  Users who held only an expired access token cannot transparently
  recover — they must log in again with email/password (or SSO).
- API keys are NOT signed with `JWT_SECRET` — they survive untouched.

There is **no graceful overlap** in 0.5.x: the server holds exactly
one `JWT_SECRET` at a time. The procedure below minimises pain by
forcing the rotation during a low-traffic window and letting users
re-authenticate.

### Procedure

1. **Pick the window.** Late nights / weekend if you have global
   users; otherwise the lowest-traffic hour. Communicate "all users
   will be signed out at HH:MM" 24h ahead.

2. **Generate the new secret.** 32 bytes of urandom, hex-encoded:
   ```bash
   openssl rand -hex 32
   ```

3. **Roll it into the Helm Secret.**

   ```bash
   helm upgrade think-watch ./deploy/helm/think-watch \
     -n thinkwatch \
     --reuse-values \
     --set secrets.jwtSecret="$(openssl rand -hex 32)"
   ```

   The deployment template's `checksum/secret` annotation forces a
   pod restart, picking up the new value automatically.

4. **Wait for the rolling restart.** `kubectl rollout status
   deployment/think-watch-server` should show fresh pods within
   ~60s. Users hitting the gateway during the restart get one of:
   - Their old access token's request lands on an OLD pod → succeeds.
   - It lands on a NEW pod → `401`, and their client should refresh.
   - Refresh hits a NEW pod with an OLD refresh token → `401`, user
     gets bounced to the login screen.

5. **Verify no clients are stuck.** Watch
   `rate(auth_refresh_redis_error_total[5m])` and
   `rate(http_requests_total{status=~"4.."}[1m])`. The 401 spike
   should drain within ~refresh-TTL minutes; if it persists for
   hours you have non-browser clients pinned to the old token.

6. **Update the `last_rotated_at` log entry** in your secret-
   management tracker.

### Compromised-secret variant

If the secret may be in attacker hands, also:

- Rotate `redis://` password too (the attacker may have observed
  Redis blacklist traffic).
- Force-logout every user via
  `POST /api/admin/users/{id}/force-logout` for high-value accounts,
  or `UPDATE users SET pw_changed_at = now()` to invalidate every
  refresh token in one shot.
- Audit `auth_refresh_replay_total` for the previous 30 days — any
  non-zero count is a confirmed replay.

---

## ENCRYPTION_KEY

This rotates the AES-256-GCM key that wraps:

- Provider header values (in `providers.config_json` under `headers[]`)
- Bedrock `aws_secret_access_key`
- MCP OAuth `client_secret` and access / refresh tokens
- TOTP secret + recovery code blobs
- MCP user-credential access / refresh tokens

Every encrypted column carries a key-version byte (the AES-GCM
envelope format prefixes it), but the server only holds **one**
key at a time. Rotating therefore requires:

1. Decrypt every encrypted cell with the **old** key.
2. Re-encrypt each with the **new** key.
3. Atomically swap the Secret in the cluster.

There is no in-place rotation primitive in 0.5.x — re-encryption
runs as a one-shot CLI you run against the cluster's DB before the
swap. Operators MUST follow the order below.

### Procedure

1. **Schedule downtime.** ~5–10 min depending on encrypted-row
   count. Reads against the affected tables (provider list, MCP
   connect, TOTP login) must not race the re-encrypt.

2. **Take a Postgres backup** — see `backup-restore.md`. Re-encryption
   touches every row carrying envelope data; you want a known-good
   snapshot.

3. **Generate the new key.**
   ```bash
   NEW_KEY=$(openssl rand -hex 32)   # 64 hex chars = 32 bytes
   ```

4. **Stop write traffic.** Scale the gateway + console deployment to
   `replicas: 0` so no producer is mid-flight encrypting with the
   old key:
   ```bash
   kubectl -n thinkwatch scale deployment/think-watch-server --replicas=0
   kubectl -n thinkwatch rollout status deployment/think-watch-server
   ```

5. **Run the re-encrypt CLI** (`make rotate-encryption-key`, which
   wraps `cargo run -p think-watch-server --bin rotate-key --
   --old <OLD> --new <NEW>`). Per row it:
   - reads ciphertext
   - decrypts with `--old`
   - re-encrypts with `--new`
   - writes back inside a single transaction per row

   The CLI exits non-zero on any decrypt failure, leaving every
   succeeded row already in the new key envelope (idempotent per
   row). Re-running the CLI with the same `--old`/`--new` pair is
   safe — already-rotated rows are detected via a probe decrypt.

6. **Swap the Secret.**
   ```bash
   helm upgrade think-watch ./deploy/helm/think-watch \
     -n thinkwatch \
     --reuse-values \
     --set secrets.encryptionKey="$NEW_KEY"
   ```

7. **Scale back up.**
   ```bash
   kubectl -n thinkwatch scale deployment/think-watch-server --replicas=1
   kubectl -n thinkwatch rollout status deployment/think-watch-server
   ```

8. **Verify.** Trigger one MCP `tools/call`, one provider request,
   one TOTP login. Each path exercises a different
   encrypted-at-rest field. Failures here mean the rotation missed
   a row — restore from the Postgres snapshot and investigate.

### Why we don't support online rotation in 0.5.x

A dual-key window would require keying every row with a `key_id`
column and the server holding multiple keys live. That's planned for
`1.x`. Until then, the offline rotation above is the supported path
— mark this on the roadmap and skip "creative" approaches.
