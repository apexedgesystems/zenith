//! Authentication primitives: credential verification, token
//! mint/validate, which credential a request presents, and boot-time
//! config validation. The axum middleware and handlers in the binary
//! are thin wrappers over these functions (and over core::session for
//! browser sessions) so the security-critical logic lives where the
//! test suite runs.

use axum::http::{header, HeaderMap};
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

/// The credential a request presents. The middleware honours them in
/// this order -- a bearer header, then a socket ticket on the query
/// string, then the browser session cookie -- and the first one
/// present decides: a bad bearer token never falls back to a cookie.
#[derive(Debug, PartialEq, Eq)]
pub enum Credential<'a> {
    Bearer(&'a str),
    Ticket(&'a str),
    Cookie(&'a str),
    None,
}

/// Which credential the request presents (see [`Credential`]).
pub fn credential<'a>(headers: &'a HeaderMap, query: Option<&'a str>) -> Credential<'a> {
    if let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
    {
        return Credential::Bearer(token);
    }
    let ticket = query.and_then(|q| {
        q.split('&').find_map(|kv| {
            let (name, value) = kv.split_once('=')?;
            (name == "ticket").then_some(value)
        })
    });
    if let Some(ticket) = ticket {
        return Credential::Ticket(ticket);
    }
    match crate::core::session::cookie_value(headers) {
        Some(id) => Credential::Cookie(id),
        None => Credential::None,
    }
}

/// The actor recorded for a refused sign-in: the submitted name when
/// it is the configured user, otherwise a fixed marker, so a password
/// typed into the user field never lands in the audit log.
pub fn refused_sign_in_actor<'a>(submitted: &'a str, configured: &str) -> &'a str {
    if submitted == configured {
        submitted
    } else {
        "(unknown user)"
    }
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
    if auth.session_max_hours == 0 {
        errors.push(
            "[auth] session_max_hours is 0; a browser session needs an absolute limit \
             of at least 1 hour (default 10)"
                .to_string(),
        );
    } else if u64::from(auth.session_idle_min) > u64::from(auth.session_max_hours) * 60 {
        errors.push(format!(
            "[auth] session_idle_min ({} min) is longer than session_max_hours ({} h); \
             the idle limit must fit inside the absolute one (0 disables it)",
            auth.session_idle_min, auth.session_max_hours
        ));
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
            ..AuthSection::default()
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

    /// @test Session limits: an absolute limit of zero and an idle limit
    /// longer than the absolute one each refuse boot naming the key; an
    /// idle limit of 0 (off) or equal to the absolute one passes; the
    /// keys are not checked while auth is off.
    #[test]
    fn boot_validation_covers_session_limits() {
        use crate::config::AuthSection;
        let mut auth = AuthSection {
            enabled: true,
            secret: "long-enough-secret-value".into(),
            password_hash: hash_password("pw").unwrap(),
            ..AuthSection::default()
        };
        assert!(boot_errors(&auth).is_empty(), "defaults pass");

        auth.session_max_hours = 0;
        let errs = boot_errors(&auth);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("session_max_hours"), "{errs:?}");

        auth.session_max_hours = 2;
        auth.session_idle_min = 121;
        let errs = boot_errors(&auth);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("session_idle_min (121 min)"), "{errs:?}");

        auth.session_idle_min = 120;
        assert!(boot_errors(&auth).is_empty(), "equal limits pass");
        auth.session_idle_min = 0;
        assert!(boot_errors(&auth).is_empty(), "idle limit off passes");

        auth.session_max_hours = 0;
        auth.enabled = false;
        assert!(boot_errors(&auth).is_empty(), "inert while auth is off");
    }

    /// @test The session keys default to a 30 minute idle limit, a 10
    /// hour absolute limit and a Secure cookie, and read from the file.
    #[test]
    fn session_keys_parse_with_defaults() {
        use crate::config::AuthSection;
        let d: AuthSection = toml::from_str("enabled = true").unwrap();
        assert_eq!(
            (d.session_idle_min, d.session_max_hours, d.cookie_secure),
            (30, 10, true)
        );
        let set: AuthSection =
            toml::from_str("session_idle_min = 0\nsession_max_hours = 12\ncookie_secure = false\n")
                .unwrap();
        assert_eq!(
            (
                set.session_idle_min,
                set.session_max_hours,
                set.cookie_secure
            ),
            (0, 12, false)
        );
    }

    /// @test Credential precedence: a bearer header wins over a query
    /// ticket and a cookie, a ticket wins over a cookie, a cookie is
    /// used alone, and a non-bearer Authorization header or a valueless
    /// ticket parameter counts as absent.
    #[test]
    fn credential_precedence_is_bearer_then_ticket_then_cookie() {
        use axum::http::HeaderValue;
        let mut all = HeaderMap::new();
        all.insert("authorization", HeaderValue::from_static("Bearer tok"));
        all.insert("cookie", HeaderValue::from_static("zenith_session=sid"));
        let q = Some("a=1&ticket=tik");
        assert_eq!(credential(&all, q), Credential::Bearer("tok"));

        let mut ticket_and_cookie = HeaderMap::new();
        ticket_and_cookie.insert("cookie", HeaderValue::from_static("zenith_session=sid"));
        assert_eq!(credential(&ticket_and_cookie, q), Credential::Ticket("tik"));
        assert_eq!(
            credential(&ticket_and_cookie, None),
            Credential::Cookie("sid")
        );

        let mut basic = HeaderMap::new();
        basic.insert("authorization", HeaderValue::from_static("Basic Zm9v"));
        assert_eq!(credential(&basic, Some("ticket")), Credential::None);
        assert_eq!(credential(&HeaderMap::new(), None), Credential::None);
    }

    /// @test A refused sign-in is attributed to the submitted name only
    /// when it is the configured user; anything else (a typo, or a
    /// password typed into the user field) becomes a fixed marker.
    #[test]
    fn refused_sign_in_never_records_arbitrary_text() {
        assert_eq!(refused_sign_in_actor("admin", "admin"), "admin");
        assert_eq!(refused_sign_in_actor("hunter2!", "admin"), "(unknown user)");
        assert_eq!(refused_sign_in_actor("", "admin"), "(unknown user)");
    }
}
