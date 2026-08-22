//! Authentication primitives: credential verification, token
//! mint/validate, and boot-time config validation. The axum middleware
//! and handlers in the binary are thin wrappers over these functions
//! so the security-critical logic lives where the test suite runs.

use serde::Deserialize;

/// Token time-to-live for interactive logins.
pub const TOKEN_TTL_SECS: i64 = 86_400;
/// Ticket time-to-live for WebSocket upgrades: long enough to open a
/// socket, short enough that a logged query string is stale before
/// anyone reads the log.
pub const WS_TICKET_TTL_SECS: i64 = 30;
/// The shipped placeholder signing secret; booting with it while auth
/// is enabled is a fatal misconfiguration.
pub const DEFAULT_SECRET: &str = "change-me-in-production";

#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    #[serde(default)]
    ws: bool,
}

/// Verify a login against the configured username and argon2 PHC hash.
/// The signing secret is never part of this comparison.
pub fn verify_credentials(
    username: &str,
    password: &str,
    cfg_username: &str,
    cfg_password_hash: &str,
) -> bool {
    use argon2::{Argon2, PasswordHash, PasswordVerifier};
    username == cfg_username
        && PasswordHash::new(cfg_password_hash)
            .map(|parsed| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &parsed)
                    .is_ok()
            })
            .unwrap_or(false)
}

/// Hash a password into an argon2 PHC string for config.toml.
pub fn hash_password(password: &str) -> Result<String, String> {
    use argon2::password_hash::{rand_core::OsRng, SaltString};
    use argon2::{Argon2, PasswordHasher};
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

/// Mint a bearer token for an authenticated subject.
pub fn mint_token(sub: &str, secret: &str, now_epoch: i64) -> Result<String, String> {
    encode_claims(
        serde_json::json!({ "sub": sub, "exp": now_epoch + TOKEN_TTL_SECS }),
        secret,
    )
}

/// Mint a short-lived WebSocket ticket for an authenticated subject.
pub fn mint_ws_ticket(sub: &str, secret: &str, now_epoch: i64) -> Result<String, String> {
    encode_claims(
        serde_json::json!({ "sub": sub, "ws": true, "exp": now_epoch + WS_TICKET_TTL_SECS }),
        secret,
    )
}

fn encode_claims(claims: serde_json::Value, secret: &str) -> Result<String, String> {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| e.to_string())
}

/// Validate a token and return the authenticated subject.
///
/// `from_query` marks credentials that arrived on a query string:
/// only ws tickets are accepted there, so a long-lived bearer token
/// can never end up in request logs.
pub fn validate_token(token: &str, secret: &str, from_query: bool) -> Result<String, String> {
    let mut validation = jsonwebtoken::Validation::default();
    validation.set_required_spec_claims(&["exp", "sub"]);
    let decoded = jsonwebtoken::decode::<Claims>(
        token,
        &jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .map_err(|e| format!("invalid token: {}", e))?;

    if from_query && !decoded.claims.ws {
        return Err("query credentials must be ws tickets (POST /api/auth/ws-ticket)".into());
    }
    Ok(decoded.claims.sub)
}

/// Boot-time validation: every reason the enabled auth config is
/// unusable. Empty means safe to serve.
pub fn boot_errors(auth: &crate::config::AuthSection) -> Vec<String> {
    let mut errors = Vec::new();
    if !auth.enabled {
        return errors;
    }
    if auth.secret == DEFAULT_SECRET || auth.secret.len() < 16 {
        errors.push("[auth] secret is the default or shorter than 16 chars".to_string());
    }
    if auth.password_hash.is_empty() {
        errors.push(
            "[auth] password_hash is empty (generate with zenith --hash-password)".to_string(),
        );
    } else if argon2::PasswordHash::new(&auth.password_hash).is_err() {
        errors.push("[auth] password_hash is not a valid PHC string".to_string());
    }
    errors
}

/* ----------------------------- Tests ----------------------------- */

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "unit-test-secret-of-decent-length";

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    /// @test A hashed password verifies against itself and rejects a
    /// wrong password and a wrong username.
    #[test]
    fn credentials_round_trip() {
        let hash = hash_password("hunter2!").unwrap();
        assert!(verify_credentials("ops", "hunter2!", "ops", &hash));
        assert!(!verify_credentials("ops", "wrong", "ops", &hash));
        assert!(!verify_credentials("admin", "hunter2!", "ops", &hash));
        assert!(!verify_credentials(
            "ops",
            "hunter2!",
            "ops",
            "not-a-phc-string"
        ));
    }

    /// @test A minted token validates from the header path and returns
    /// its subject; the wrong secret rejects it.
    #[test]
    fn token_mint_and_validate() {
        let token = mint_token("ops", SECRET, now()).unwrap();
        assert_eq!(validate_token(&token, SECRET, false).unwrap(), "ops");
        assert!(validate_token(&token, "other-secret-that-is-long", false).is_err());
    }

    /// @test An expired token is rejected.
    #[test]
    fn expired_token_rejected() {
        let token = mint_token("ops", SECRET, now() - 2 * TOKEN_TTL_SECS).unwrap();
        assert!(validate_token(&token, SECRET, false).is_err());
    }

    /// @test Query-string credentials must be ws tickets: a full bearer
    /// token is rejected from the query path but accepted from the
    /// header path, and a ws ticket passes the query path.
    #[test]
    fn query_path_requires_ws_ticket() {
        let bearer = mint_token("ops", SECRET, now()).unwrap();
        assert!(validate_token(&bearer, SECRET, true).is_err());
        assert!(validate_token(&bearer, SECRET, false).is_ok());

        let ticket = mint_ws_ticket("ops", SECRET, now()).unwrap();
        assert_eq!(validate_token(&ticket, SECRET, true).unwrap(), "ops");
    }

    /// @test Tokens without required claims are rejected (a token
    /// signed correctly but missing sub).
    #[test]
    fn missing_sub_rejected() {
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &serde_json::json!({ "exp": now() + 1000 }),
            &jsonwebtoken::EncodingKey::from_secret(SECRET.as_bytes()),
        )
        .unwrap();
        assert!(validate_token(&token, SECRET, false).is_err());
    }

    /// @test Boot validation: disabled auth is always fine; enabled
    /// auth rejects the default secret, a short secret, an empty hash,
    /// and a malformed hash -- and passes a proper configuration.
    #[test]
    fn boot_validation_covers_each_misconfiguration() {
        use crate::config::AuthSection;
        let good_hash = hash_password("pw").unwrap();

        let mut auth = AuthSection {
            enabled: false,
            secret: DEFAULT_SECRET.to_string(),
            username: "admin".into(),
            password_hash: String::new(),
        };
        assert!(
            boot_errors(&auth).is_empty(),
            "disabled auth is never fatal"
        );

        auth.enabled = true;
        assert_eq!(boot_errors(&auth).len(), 2, "default secret + empty hash");

        auth.secret = "long-enough-secret-value".into();
        auth.password_hash = "garbage".into();
        assert_eq!(boot_errors(&auth).len(), 1, "malformed hash");

        auth.password_hash = good_hash;
        assert!(boot_errors(&auth).is_empty(), "proper config passes");
    }
}
