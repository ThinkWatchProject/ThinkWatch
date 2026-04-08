use chrono::{Duration, Utc};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Audience claim — identifies the application that consumes this token.
/// Hardcoded to a stable string so any token issued by another service
/// using the same secret won't validate here.
pub const JWT_AUDIENCE: &str = "thinkwatch";
/// Issuer claim — identifies the application that issued this token.
pub const JWT_ISSUER: &str = "thinkwatch";

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Claims {
    pub sub: Uuid,
    pub email: String,
    /// Flat list of role names the user holds (system + custom, union).
    /// Used by the UI for badges and by legacy name-based checks. Not
    /// load-bearing for authorization — see `permissions`.
    pub roles: Vec<String>,
    /// Union of all `rbac_roles.permissions` across every role the user
    /// has. This is the **authoritative** set for runtime authorization
    /// decisions — middleware and handlers check membership against
    /// this field, not against `roles`.
    #[serde(default)]
    pub permissions: Vec<String>,
    pub exp: i64,
    pub iat: i64,
    pub token_type: String, // "access" or "refresh"
    /// Audience — must equal `JWT_AUDIENCE` on verify.
    #[serde(default)]
    pub aud: String,
    /// Issuer — must equal `JWT_ISSUER` on verify.
    #[serde(default)]
    pub iss: String,
}

pub struct JwtManager {
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
}

impl JwtManager {
    pub fn new(secret: &str) -> Self {
        Self {
            encoding_key: EncodingKey::from_secret(secret.as_bytes()),
            decoding_key: DecodingKey::from_secret(secret.as_bytes()),
        }
    }

    pub fn create_access_token(
        &self,
        user_id: Uuid,
        email: &str,
        roles: Vec<String>,
        permissions: Vec<String>,
    ) -> anyhow::Result<String> {
        self.create_access_token_with_ttl(user_id, email, roles, permissions, 900)
    }

    pub fn create_access_token_with_ttl(
        &self,
        user_id: Uuid,
        email: &str,
        roles: Vec<String>,
        permissions: Vec<String>,
        ttl_secs: i64,
    ) -> anyhow::Result<String> {
        let now = Utc::now();
        let claims = Claims {
            sub: user_id,
            email: email.to_string(),
            roles,
            permissions,
            exp: (now + Duration::seconds(ttl_secs)).timestamp(),
            iat: now.timestamp(),
            token_type: "access".to_string(),
            aud: JWT_AUDIENCE.to_string(),
            iss: JWT_ISSUER.to_string(),
        };
        // Pin algorithm to HS256 — Header::default() also returns HS256 today
        // but we set it explicitly to defend against future default drift.
        Ok(encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &self.encoding_key,
        )?)
    }

    pub fn create_refresh_token(
        &self,
        user_id: Uuid,
        email: &str,
        roles: Vec<String>,
        permissions: Vec<String>,
    ) -> anyhow::Result<String> {
        self.create_refresh_token_with_ttl(user_id, email, roles, permissions, 7)
    }

    pub fn create_refresh_token_with_ttl(
        &self,
        user_id: Uuid,
        email: &str,
        roles: Vec<String>,
        permissions: Vec<String>,
        ttl_days: i64,
    ) -> anyhow::Result<String> {
        let now = Utc::now();
        let claims = Claims {
            sub: user_id,
            email: email.to_string(),
            roles,
            permissions,
            exp: (now + Duration::days(ttl_days)).timestamp(),
            iat: now.timestamp(),
            token_type: "refresh".to_string(),
            aud: JWT_AUDIENCE.to_string(),
            iss: JWT_ISSUER.to_string(),
        };
        Ok(encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &self.encoding_key,
        )?)
    }

    pub fn verify_token(&self, token: &str) -> anyhow::Result<Claims> {
        // Pin to HS256 only and require the hardcoded audience + issuer.
        // Without aud/iss enforcement, a token from another service that
        // happens to share the JWT secret would validate here, breaking
        // multi-tenant isolation.
        let mut validation = Validation::new(Algorithm::HS256);
        validation.leeway = 30; // Allow 30s clock skew
        validation.set_audience(&[JWT_AUDIENCE]);
        validation.set_issuer(&[JWT_ISSUER]);
        let token_data = decode::<Claims>(token, &self.decoding_key, &validation)?;
        Ok(token_data.claims)
    }
}

/// Compute a SHA-256 hex hash of a token (for blacklist keys; never store raw tokens).
pub fn sha2_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Add a token hash to the Redis blacklist with a TTL.
pub async fn revoke_token(
    redis: &fred::clients::Client,
    token_hash: &str,
    ttl_secs: i64,
) -> anyhow::Result<()> {
    use fred::interfaces::KeysInterface;
    let key = format!("jwt_blacklist:{token_hash}");
    // Use atomic SET EX to avoid a race condition where a crash between
    // SET and EXPIRE would leave the key without a TTL (never expiring).
    let _: () = redis
        .set(
            &key,
            "1",
            Some(fred::types::Expiration::EX(ttl_secs)),
            None,
            false,
        )
        .await?;
    Ok(())
}

/// Check whether a token hash has been revoked.
pub async fn is_revoked(redis: &fred::clients::Client, token_hash: &str) -> bool {
    use fred::interfaces::KeysInterface;
    let key = format!("jwt_blacklist:{token_hash}");
    redis.exists::<u8, _>(&key).await.unwrap_or(0) > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_jwt_manager() -> JwtManager {
        JwtManager::new("test-secret-key-for-unit-tests")
    }

    fn test_user_id() -> Uuid {
        Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap()
    }

    #[test]
    fn access_token_roundtrip() {
        let mgr = test_jwt_manager();
        let uid = test_user_id();
        let token = mgr
            .create_access_token(
                uid,
                "alice@example.com",
                vec!["admin".into()],
                vec!["users:read".into(), "users:update".into()],
            )
            .expect("create should succeed");

        let claims = mgr.verify_token(&token).expect("verify should succeed");
        assert_eq!(claims.sub, uid);
        assert_eq!(claims.email, "alice@example.com");
        assert_eq!(claims.token_type, "access");
        assert_eq!(claims.roles, vec!["admin"]);
        assert_eq!(claims.permissions, vec!["users:read", "users:update"]);
    }

    #[test]
    fn expired_token_fails_verification() {
        let mgr = test_jwt_manager();
        let uid = test_user_id();

        // Manually create an already-expired token
        let now = Utc::now();
        let claims = Claims {
            sub: uid,
            email: "bob@example.com".into(),
            roles: vec![],
            permissions: vec![],
            exp: (now - Duration::hours(1)).timestamp(),
            iat: (now - Duration::hours(2)).timestamp(),
            token_type: "access".into(),
            aud: JWT_AUDIENCE.into(),
            iss: JWT_ISSUER.into(),
        };
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(b"test-secret-key-for-unit-tests"),
        )
        .unwrap();

        let result = mgr.verify_token(&token);
        assert!(result.is_err(), "expired token must fail verification");
    }

    #[test]
    fn token_with_wrong_audience_fails() {
        let mgr = test_jwt_manager();
        let uid = test_user_id();
        let now = Utc::now();
        let claims = Claims {
            sub: uid,
            email: "x@y.com".into(),
            roles: vec![],
            permissions: vec![],
            exp: (now + Duration::hours(1)).timestamp(),
            iat: now.timestamp(),
            token_type: "access".into(),
            aud: "some-other-app".into(),
            iss: JWT_ISSUER.into(),
        };
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(b"test-secret-key-for-unit-tests"),
        )
        .unwrap();
        assert!(
            mgr.verify_token(&token).is_err(),
            "token with foreign aud must be rejected"
        );
    }

    #[test]
    fn token_with_wrong_issuer_fails() {
        let mgr = test_jwt_manager();
        let uid = test_user_id();
        let now = Utc::now();
        let claims = Claims {
            sub: uid,
            email: "x@y.com".into(),
            roles: vec![],
            permissions: vec![],
            exp: (now + Duration::hours(1)).timestamp(),
            iat: now.timestamp(),
            token_type: "access".into(),
            aud: JWT_AUDIENCE.into(),
            iss: "some-other-issuer".into(),
        };
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(b"test-secret-key-for-unit-tests"),
        )
        .unwrap();
        assert!(
            mgr.verify_token(&token).is_err(),
            "token with foreign iss must be rejected"
        );
    }

    #[test]
    fn refresh_token_has_correct_type() {
        let mgr = test_jwt_manager();
        let uid = test_user_id();
        let token = mgr
            .create_refresh_token(uid, "carol@example.com", vec!["viewer".into()], vec![])
            .expect("create should succeed");

        let claims = mgr.verify_token(&token).expect("verify should succeed");
        assert_eq!(claims.token_type, "refresh");
    }

    #[test]
    fn access_token_custom_ttl() {
        let mgr = test_jwt_manager();
        let uid = test_user_id();
        let now = Utc::now().timestamp();
        let token = mgr
            .create_access_token_with_ttl(uid, "ttl@example.com", vec![], vec![], 60)
            .expect("create should succeed");

        let claims = mgr.verify_token(&token).expect("verify should succeed");
        assert_eq!(claims.token_type, "access");
        // exp should be approximately now + 60 seconds (allow 5s tolerance)
        let expected_exp = now + 60;
        assert!(
            (claims.exp - expected_exp).abs() <= 5,
            "exp {} should be ~{} (now + 60s)",
            claims.exp,
            expected_exp
        );
    }

    #[test]
    fn refresh_token_custom_ttl() {
        let mgr = test_jwt_manager();
        let uid = test_user_id();
        let now = Utc::now().timestamp();
        let token = mgr
            .create_refresh_token_with_ttl(uid, "ttl@example.com", vec![], vec![], 1)
            .expect("create should succeed");

        let claims = mgr.verify_token(&token).expect("verify should succeed");
        assert_eq!(claims.token_type, "refresh");
        // exp should be approximately now + 1 day (86400 seconds, allow 5s tolerance)
        let expected_exp = now + 86400;
        assert!(
            (claims.exp - expected_exp).abs() <= 5,
            "exp {} should be ~{} (now + 1 day)",
            claims.exp,
            expected_exp
        );
    }
}
