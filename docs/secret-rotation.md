# Secret Rotation Guide

This document describes procedures for rotating the various secrets used by ThinkWatch.

## JWT Secret Rotation

The JWT secret (`JWT_SECRET`) is used to sign and verify access/refresh tokens.

### Procedure

1. **Generate a new secret** (at least 32 characters):
   ```bash
   openssl rand -hex 32
   ```

2. **Plan a maintenance window** — rotating the JWT secret invalidates all existing tokens immediately.

3. **Update the environment variable** in your deployment (`.env.production`, Kubernetes Secret, etc.):
   ```
   JWT_SECRET=<new-secret>
   ```

4. **Restart all ThinkWatch server instances** simultaneously.

5. **Users will need to re-login** — existing access and refresh tokens become invalid.

### Impact
- All active sessions are terminated
- API consumers using JWT tokens must re-authenticate
- API keys (Bearer `ab-*`) are **not affected** (they use hash-based auth)

---

## Encryption Key Rotation

The encryption key (`ENCRYPTION_KEY`) is used to encrypt provider API keys and MCP server auth secrets stored in the database (AES-256-GCM).

### Procedure

1. **Generate a new 32-byte key** (64 hex characters):
   ```bash
   openssl rand -hex 32
   ```

2. **Re-encrypt all provider API keys and MCP secrets**:
   ```sql
   -- This must be done programmatically.
   -- Export encrypted values, decrypt with old key, re-encrypt with new key, update DB.
   ```

   A migration script is recommended:
   ```rust
   let old_key = parse_encryption_key(&old_hex)?;
   let new_key = parse_encryption_key(&new_hex)?;
   
   // For each provider
   let decrypted = decrypt(&provider.api_key_encrypted, &old_key)?;
   let re_encrypted = encrypt(&decrypted, &new_key)?;
   // UPDATE providers SET api_key_encrypted = $1 WHERE id = $2
   ```

3. **Update the environment variable**:
   ```
   ENCRYPTION_KEY=<new-64-hex-chars>
   ```

4. **Restart all instances**.

### Impact
- If you forget to re-encrypt existing secrets, they become unreadable
- New provider/MCP secrets will use the new key
- Always test decryption with the new key before deploying

---

## API Key Rotation

API keys can be rotated without downtime using the built-in rotation API.

### Via API

```bash
# Rotate a key (old key stays active during grace period)
curl -X POST /api/keys/{key_id}/rotate \
  -H "Authorization: Bearer <access_token>" \
  -H "X-Signature-Timestamp: ..." \
  -H "X-Signature-Nonce: ..." \
  -H "X-Signature: hmac-sha256:..."

# Response includes the new plaintext key
{
  "id": "new-key-uuid",
  "key": "ab-...",
  "name": "My Key (rotated)",
  "key_prefix": "ab-..."
}
```

### Via Web UI

1. Navigate to **AI Gateway → API Keys**
2. Click the **Rotate** button on the key row
3. Confirm the rotation in the dialog
4. Copy the new key immediately (shown only once)
5. Update your applications with the new key
6. The old key remains active for the configured grace period (default: 24 hours)

### Auto-Rotation

Configure automatic rotation in **Settings → API Key Policies**:
- **Rotation Period (days)**: Set to > 0 to enable auto-rotation
- **Grace Period (hours)**: How long the old key remains active after rotation

---

## OIDC Client Secret Rotation

The OIDC client secret is used to authenticate with your identity provider (e.g., Zitadel).

### Procedure

1. **Generate a new client secret** in your OIDC provider's admin console.

2. **Update the environment variable**:
   ```
   OIDC_CLIENT_SECRET=<new-secret>
   ```

3. **Restart all ThinkWatch instances** — OIDC discovery runs at startup.

### Impact
- SSO logins may fail briefly during the restart
- Existing JWT tokens remain valid until they expire
- Users with active sessions are not affected

---

### Redis Signing Keys

Signing keys are used for HMAC-SHA256 request signature verification on state-changing requests (POST, PUT, PATCH, DELETE). They are:

- Auto-generated on login and token refresh (32 random bytes, hex-encoded)
- Stored in Redis with 24-hour TTL (matching refresh token lifetime)
- **Bound to client IP** — the IP address at login is stored alongside the key; requests from a different IP are rejected
- Delivered to the client via httpOnly cookie (`signing_key`)

**Rotation:** Signing keys auto-rotate on every login and token refresh. To force rotation for a specific user, use "Force Logout" in the admin panel, which invalidates all their sessions and signing keys.

No manual rotation is needed.

---

## Best Practices

1. **Schedule rotations during low-traffic periods** when possible
2. **Test in staging** before rotating production secrets
3. **Document the rotation** in your operations log
4. **Monitor for errors** after rotation — check `/api/health` and application logs
5. **Use different secrets per environment** (dev, staging, production)
6. **Store secrets in a vault** (HashiCorp Vault, AWS Secrets Manager, etc.) rather than plain files
7. **Log forwarder credentials** — if using Kafka, HTTP webhook, or Syslog with authentication, rotate those credentials via Admin > Log Forwarding and test connectivity after rotation
