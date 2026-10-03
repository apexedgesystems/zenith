//! Browser sessions: a server-side record per sign-in, addressed by an
//! opaque random id that travels only in an HttpOnly cookie.
//!
//! The store is the one authority on whether a session is alive. It
//! keeps its own lock, holds sessions by a SHA-256 of the id (never
//! the id itself, in memory or at rest), writes through to the
//! `sessions` table so sessions survive a restart, and tells each
//! session's telemetry sockets when the session ends. Deadlines are
//! derived from two stored facts (sign-in and last keep-alive) and
//! the configured limits, in one place. Every function that depends
//! on time takes the time as an argument.
//!
//! Also here: the cookie's one name and attribute set, and the
//! same-origin rule that cookie-authenticated unsafe requests and
//! socket upgrades must pass.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use argon2::password_hash::rand_core::{OsRng, RngCore};
use axum::http::{header, HeaderMap, Method};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::config::AuthSection;
use crate::storage::telemetry_db::{DbError, SessionRow, TelemetryDb};

/* ----------------------------- Constants ----------------------------- */

/// The session cookie's name. Setting, clearing and reading the cookie
/// all go through this module.
pub const COOKIE_NAME: &str = "zenith_session";

/// How often the expiry sweep runs: the most a socket can outlive its
/// session's deadline.
pub const SWEEP_EVERY_MS: u64 = 1_000;

/// Random bytes in a session id (hex-encoded into the cookie).
const ID_BYTES: usize = 32;

/* ----------------------------- Types ----------------------------- */

/// The configured limits, in milliseconds. An idle limit of 0 means
/// the session has none and lives until its absolute deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionPolicy {
    pub idle_ms: u64,
    pub absolute_ms: u64,
}

/// Why a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// No keep-alive within the idle limit.
    Idle,
    /// The absolute limit after sign-in.
    Absolute,
    /// Signed out.
    SignedOut,
}

/// A live session as callers see it: a snapshot taken from the store
/// at one instant, for the request that asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// SHA-256 of the cookie value, hex: the store's key and the only
    /// form of the id that is ever kept.
    pub key: String,
    pub user: Arc<str>,
    /// None when the idle limit is off.
    pub idle_deadline_ms: Option<u64>,
    pub absolute_deadline_ms: u64,
}

/// The body every session endpoint answers with. With auth off it is
/// `{"auth_enabled": false}`; for a live session it adds the user, both
/// deadlines (`idle_deadline_ms` is null without an idle limit) and the
/// server's clock, so a client can time its prompts against the
/// server instead of its own clock.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionStatus {
    pub auth_enabled: bool,
    #[serde(flatten)]
    pub session: Option<LiveSession>,
}

/// The live part of [`SessionStatus`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LiveSession {
    pub user: String,
    pub idle_deadline_ms: Option<u64>,
    pub absolute_deadline_ms: u64,
    pub server_time_ms: u64,
}

/// The session store. See the module documentation.
pub struct SessionStore {
    db: Arc<TelemetryDb>,
    policy: SessionPolicy,
    live: Mutex<HashMap<String, Entry>>,
}

/// One live session in memory: the stored facts plus the channel its
/// telemetry sockets wait on.
struct Entry {
    user: Arc<str>,
    created_ms: u64,
    refreshed_ms: u64,
    ended: watch::Sender<Option<EndReason>>,
}

/* ----------------------------- Helpers ----------------------------- */

impl Entry {
    fn new(user: Arc<str>, created_ms: u64, refreshed_ms: u64) -> Self {
        Self {
            user,
            created_ms,
            refreshed_ms,
            ended: watch::channel(None).0,
        }
    }
}

/// A fresh session id: 256 bits from the operating system's generator.
fn new_id() -> String {
    let mut bytes = [0u8; ID_BYTES];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// The store key for a cookie value.
fn key_of(id: &str) -> String {
    hex::encode(Sha256::digest(id.as_bytes()))
}

fn cookie_attributes(secure: bool) -> &'static str {
    if secure {
        "Path=/; HttpOnly; SameSite=Strict; Secure"
    } else {
        "Path=/; HttpOnly; SameSite=Strict"
    }
}

/* ----------------------------- API ----------------------------- */

impl SessionPolicy {
    /// The limits `[auth]` configures.
    pub fn from_config(auth: &AuthSection) -> Self {
        Self {
            idle_ms: u64::from(auth.session_idle_min) * 60_000,
            absolute_ms: u64::from(auth.session_max_hours) * 3_600_000,
        }
    }

    /// The instant a session signed in at `created_ms` ends regardless
    /// of activity.
    pub fn absolute_deadline(&self, created_ms: u64) -> u64 {
        created_ms.saturating_add(self.absolute_ms)
    }

    /// The idle deadline after a keep-alive at `refreshed_ms`, clamped
    /// at the absolute deadline; None without an idle limit.
    pub fn idle_deadline(&self, created_ms: u64, refreshed_ms: u64) -> Option<u64> {
        (self.idle_ms > 0).then(|| {
            refreshed_ms
                .saturating_add(self.idle_ms)
                .min(self.absolute_deadline(created_ms))
        })
    }

    /// Why the session has ended by `now_ms`, or None while it lives.
    /// A session ends at its earlier deadline; an idle deadline clamped
    /// onto the absolute one counts as the absolute limit.
    pub fn ended_by(&self, created_ms: u64, refreshed_ms: u64, now_ms: u64) -> Option<EndReason> {
        let absolute = self.absolute_deadline(created_ms);
        let effective = self
            .idle_deadline(created_ms, refreshed_ms)
            .unwrap_or(absolute);
        if now_ms < effective {
            None
        } else if effective < absolute {
            Some(EndReason::Idle)
        } else {
            Some(EndReason::Absolute)
        }
    }
}

impl EndReason {
    /// Text for a socket's close frame and the audit record.
    pub fn describe(self) -> &'static str {
        match self {
            EndReason::Idle => "session ended: idle limit",
            EndReason::Absolute => "session ended: absolute limit",
            EndReason::SignedOut => "session ended: signed out",
        }
    }
}

impl SessionStatus {
    /// The answer when auth is off.
    pub fn disabled() -> Self {
        Self {
            auth_enabled: false,
            session: None,
        }
    }

    /// The answer for a live session, stamped with the server's clock.
    pub fn live(session: &Session, now_ms: u64) -> Self {
        Self {
            auth_enabled: true,
            session: Some(LiveSession {
                user: session.user.to_string(),
                idle_deadline_ms: session.idle_deadline_ms,
                absolute_deadline_ms: session.absolute_deadline_ms,
                server_time_ms: now_ms,
            }),
        }
    }
}

impl SessionStore {
    /// Open the store over the database, loading every session still
    /// alive at `now_ms` and deleting the rows of those that ended
    /// while the server was down.
    pub fn open(db: Arc<TelemetryDb>, policy: SessionPolicy, now_ms: u64) -> Result<Self, DbError> {
        let mut live = HashMap::new();
        for row in db.load_sessions()? {
            if policy
                .ended_by(row.created_ms, row.refreshed_ms, now_ms)
                .is_some()
            {
                db.delete_session(&row.key)?;
                continue;
            }
            live.insert(
                row.key,
                Entry::new(Arc::from(row.user), row.created_ms, row.refreshed_ms),
            );
        }
        Ok(Self {
            db,
            policy,
            live: Mutex::new(live),
        })
    }

    /// Start a session for `user`. Returns the id for the cookie (the
    /// only time the raw id exists) and the session. The row is written
    /// before the session becomes usable.
    pub fn create(&self, user: &str, now_ms: u64) -> Result<(String, Session), DbError> {
        let id = new_id();
        let key = key_of(&id);
        self.db.insert_session(&SessionRow {
            key: key.clone(),
            user: user.to_string(),
            created_ms: now_ms,
            refreshed_ms: now_ms,
        })?;
        let entry = Entry::new(Arc::from(user), now_ms, now_ms);
        let session = self.view(&key, &entry);
        self.lock().insert(key, entry);
        Ok((id, session))
    }

    /// The live session a cookie value names at `now_ms`. Memory only
    /// and read only: looking a session up never extends it, so
    /// requests, polling and socket traffic cannot keep it alive.
    pub fn lookup(&self, cookie_value: &str, now_ms: u64) -> Option<Session> {
        let key = key_of(cookie_value);
        let live = self.lock();
        let entry = live.get(&key)?;
        if self
            .policy
            .ended_by(entry.created_ms, entry.refreshed_ms, now_ms)
            .is_some()
        {
            return None;
        }
        Some(self.view(&key, entry))
    }

    /// The explicit keep-alive: restart the idle limit at `now_ms`,
    /// never past the absolute deadline. None when the session is not
    /// alive.
    pub fn refresh(&self, key: &str, now_ms: u64) -> Result<Option<Session>, DbError> {
        {
            let live = self.lock();
            match live.get(key) {
                Some(e)
                    if self
                        .policy
                        .ended_by(e.created_ms, e.refreshed_ms, now_ms)
                        .is_none() => {}
                _ => return Ok(None),
            }
        }
        self.db.touch_session(key, now_ms)?;
        let mut live = self.lock();
        let Some(entry) = live.get_mut(key) else {
            return Ok(None);
        };
        entry.refreshed_ms = entry.refreshed_ms.max(now_ms);
        Ok(Some(self.view(key, entry)))
    }

    /// End a session (sign-out). The row is deleted first, so an end
    /// that could not be recorded fails instead of coming back at the
    /// next boot. Returns the session's user when it was alive.
    pub fn end(&self, key: &str, reason: EndReason) -> Result<Option<Arc<str>>, DbError> {
        self.db.delete_session(key)?;
        let entry = self.lock().remove(key);
        Ok(entry.map(|e| {
            e.ended.send_replace(Some(reason));
            e.user
        }))
    }

    /// End every session whose deadline has passed at `now_ms` and
    /// notify its sockets. Rows are deleted best-effort: an expired row
    /// is dead by its times whether or not the delete lands.
    pub fn sweep(&self, now_ms: u64) -> Vec<(Arc<str>, EndReason)> {
        let expired: Vec<(String, Entry, EndReason)> = {
            let mut live = self.lock();
            let keys: Vec<(String, EndReason)> = live
                .iter()
                .filter_map(|(k, e)| {
                    self.policy
                        .ended_by(e.created_ms, e.refreshed_ms, now_ms)
                        .map(|r| (k.clone(), r))
                })
                .collect();
            keys.into_iter()
                .filter_map(|(k, r)| live.remove(&k).map(|e| (k, e, r)))
                .collect()
        };
        let mut ended = Vec::with_capacity(expired.len());
        for (key, entry, reason) in expired {
            entry.ended.send_replace(Some(reason));
            if let Err(e) = self.db.delete_session(&key) {
                tracing::warn!("expired session row not deleted: {}", e);
            }
            ended.push((entry.user, reason));
        }
        ended
    }

    /// A receiver that resolves to the end reason when the session
    /// ends; None when it is not alive. Telemetry sockets wait on it.
    pub fn subscribe(&self, key: &str) -> Option<watch::Receiver<Option<EndReason>>> {
        self.lock().get(key).map(|e| e.ended.subscribe())
    }

    fn view(&self, key: &str, entry: &Entry) -> Session {
        Session {
            key: key.to_string(),
            user: entry.user.clone(),
            idle_deadline_ms: self
                .policy
                .idle_deadline(entry.created_ms, entry.refreshed_ms),
            absolute_deadline_ms: self.policy.absolute_deadline(entry.created_ms),
        }
    }

    /// The map has no multi-step invariant a panicking holder could
    /// break, so a poisoned lock is still a consistent one.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `Set-Cookie` value that stores a session id: HttpOnly, SameSite
/// Strict, Path=/, and Secure unless the deployment serves plain HTTP
/// on a trusted network. No Max-Age: the browser drops the cookie when
/// it closes, and the server's deadlines bound the session either way.
pub fn set_cookie(id: &str, secure: bool) -> String {
    format!("{COOKIE_NAME}={id}; {}", cookie_attributes(secure))
}

/// `Set-Cookie` value that removes the session cookie.
pub fn clear_cookie(secure: bool) -> String {
    format!("{COOKIE_NAME}=; Max-Age=0; {}", cookie_attributes(secure))
}

/// The session id a request's Cookie headers carry, if any.
pub fn cookie_value(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|h| h.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            let value = value.trim();
            (name.trim() == COOKIE_NAME && !value.is_empty()).then_some(value)
        })
}

/// Whether a request that authenticates with the session cookie may
/// proceed. GET, HEAD and OPTIONS pass. Any other method, or a socket
/// upgrade, must come from the console's own origin: `Sec-Fetch-Site:
/// same-origin`, or, when the browser sent no such header, an `Origin`
/// whose host and port equal `Host`. Anything else (another site, a
/// sibling origin on the same site, no origin at all) is refused.
pub fn same_origin_allows(method: &Method, headers: &HeaderMap) -> bool {
    let upgrade = headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("websocket"));
    let exempt = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    if exempt && !upgrade {
        return true;
    }
    let text = |name| headers.get(name).and_then(|v| v.to_str().ok());
    match text(header::HeaderName::from_static("sec-fetch-site")) {
        Some(site) => site.trim().eq_ignore_ascii_case("same-origin"),
        None => match (text(header::ORIGIN), text(header::HOST)) {
            (Some(origin), Some(host)) => origin
                .trim()
                .split_once("://")
                .is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host.trim())),
            _ => false,
        },
    }
}

/* ----------------------------- Tests ----------------------------- */

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use tempfile::TempDir;

    const MIN: u64 = 60_000;
    const HOUR: u64 = 3_600_000;
    /// An arbitrary sign-in instant; the store never reads a clock.
    const T0: u64 = 1_800_000_000_000;

    fn policy(idle_min: u64, max_hours: u64) -> SessionPolicy {
        SessionPolicy {
            idle_ms: idle_min * MIN,
            absolute_ms: max_hours * HOUR,
        }
    }

    fn open_store(dir: &TempDir, p: SessionPolicy, now: u64) -> SessionStore {
        let db = TelemetryDb::open(&dir.path().join("sessions.db")).unwrap();
        SessionStore::open(Arc::new(db), p, now).unwrap()
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (name, value) in pairs {
            h.append(*name, HeaderValue::from_str(value).unwrap());
        }
        h
    }

    /// @test A new session's deadlines are sign-in plus each limit, the
    /// idle one clamped at the absolute one; with the idle limit off
    /// only the absolute deadline exists.
    #[test]
    fn create_sets_both_deadlines() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir, policy(30, 10), T0);
        let (id, s) = store.create("ops", T0).unwrap();
        assert_eq!(s.user.as_ref(), "ops");
        assert_eq!(s.idle_deadline_ms, Some(T0 + 30 * MIN));
        assert_eq!(s.absolute_deadline_ms, T0 + 10 * HOUR);
        assert_eq!(store.lookup(&id, T0), Some(s));

        let no_idle = SessionPolicy {
            idle_ms: 0,
            absolute_ms: HOUR,
        };
        assert_eq!(no_idle.idle_deadline(T0, T0), None);
        assert_eq!(no_idle.ended_by(T0, T0, T0 + HOUR - 1), None);
        assert_eq!(
            no_idle.ended_by(T0, T0, T0 + HOUR),
            Some(EndReason::Absolute)
        );
        let long_idle = policy(90, 1);
        assert_eq!(long_idle.idle_deadline(T0, T0), Some(T0 + HOUR));
    }

    /// @test A session is alive until the instant of its idle deadline
    /// and gone at it; the reason is idle. The absolute limit ends it
    /// even with keep-alives, and then the reason is absolute.
    #[test]
    fn deadlines_end_the_session_with_their_reason() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir, policy(30, 1), T0);
        let (id, s) = store.create("ops", T0).unwrap();
        let idle = s.idle_deadline_ms.unwrap();
        assert!(store.lookup(&id, idle - 1).is_some());
        assert!(store.lookup(&id, idle).is_none());
        assert_eq!(store.policy.ended_by(T0, T0, idle), Some(EndReason::Idle));

        let (id2, s2) = store.create("ops", T0).unwrap();
        let mut now = T0;
        while now + 20 * MIN < T0 + HOUR {
            now += 20 * MIN;
            assert!(store.refresh(&s2.key, now).unwrap().is_some());
        }
        assert!(store.lookup(&id2, T0 + HOUR - 1).is_some());
        assert!(store.lookup(&id2, T0 + HOUR).is_none());
        assert_eq!(
            store.policy.ended_by(T0, now, T0 + HOUR),
            Some(EndReason::Absolute)
        );
    }

    /// @test A keep-alive restarts the idle limit from its own instant
    /// and is clamped at the absolute deadline; a keep-alive after the
    /// session ended is refused.
    #[test]
    fn keep_alive_extends_but_never_past_the_absolute_deadline() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir, policy(30, 1), T0);
        let (id, s) = store.create("ops", T0).unwrap();

        let r = store.refresh(&s.key, T0 + 10 * MIN).unwrap().unwrap();
        assert_eq!(r.idle_deadline_ms, Some(T0 + 40 * MIN));
        assert_eq!(r.absolute_deadline_ms, T0 + HOUR);

        let r = store.refresh(&s.key, T0 + 35 * MIN).unwrap().unwrap();
        assert_eq!(r.idle_deadline_ms, Some(T0 + HOUR), "clamped");
        assert!(store.lookup(&id, T0 + HOUR - 1).is_some());
        assert!(store.lookup(&id, T0 + HOUR).is_none());
        assert_eq!(store.refresh(&s.key, T0 + HOUR).unwrap(), None);
    }

    /// @test Looking a session up never extends it: a thousand lookups
    /// leave both deadlines where sign-in put them, and the session
    /// still ends at its idle deadline.
    #[test]
    fn lookups_never_extend_a_session() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir, policy(30, 10), T0);
        let (id, s) = store.create("ops", T0).unwrap();
        for i in 0..1000 {
            let seen = store.lookup(&id, T0 + i * 1_000).unwrap();
            assert_eq!(seen.idle_deadline_ms, s.idle_deadline_ms);
            assert_eq!(seen.absolute_deadline_ms, s.absolute_deadline_ms);
        }
        assert!(store.lookup(&id, s.idle_deadline_ms.unwrap()).is_none());
    }

    /// @test Ending a session makes its cookie useless at once, wakes
    /// its sockets with the reason, survives a reopen of the store, and
    /// answers None for a session that is already gone.
    #[test]
    fn end_is_immediate_notified_and_durable() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir, policy(30, 10), T0);
        let (id, s) = store.create("ops", T0).unwrap();
        let rx = store.subscribe(&s.key).unwrap();
        assert_eq!(*rx.borrow(), None);

        let user = store.end(&s.key, EndReason::SignedOut).unwrap();
        assert_eq!(user.as_deref(), Some("ops"));
        assert_eq!(*rx.borrow(), Some(EndReason::SignedOut));
        assert!(store.lookup(&id, T0).is_none());
        assert!(store.subscribe(&s.key).is_none());
        assert_eq!(store.end(&s.key, EndReason::SignedOut).unwrap(), None);

        drop(store);
        let reopened = open_store(&dir, policy(30, 10), T0 + MIN);
        assert!(reopened.lookup(&id, T0 + MIN).is_none());
    }

    /// @test The sweep ends exactly the sessions past a deadline at the
    /// given instant, notifies each with its own reason, and leaves the
    /// live one alone.
    #[test]
    fn sweep_ends_expired_sessions_with_their_reasons() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir, policy(30, 1), T0);
        let (_, idle_one) = store.create("a", T0).unwrap();
        let (_, kept_alive) = store.create("b", T0).unwrap();
        let (live_id, _) = store.create("c", T0 + 40 * MIN).unwrap();
        let rx_idle = store.subscribe(&idle_one.key).unwrap();
        let rx_abs = store.subscribe(&kept_alive.key).unwrap();

        store.refresh(&kept_alive.key, T0 + 29 * MIN).unwrap();
        assert!(store.sweep(T0 + 29 * MIN).is_empty());
        let ended = store.sweep(T0 + 30 * MIN);
        assert_eq!(ended, vec![(Arc::from("a"), EndReason::Idle)]);
        assert_eq!(*rx_idle.borrow(), Some(EndReason::Idle));
        assert_eq!(*rx_abs.borrow(), None);

        store.refresh(&kept_alive.key, T0 + 58 * MIN).unwrap();
        let ended = store.sweep(T0 + HOUR);
        assert_eq!(ended, vec![(Arc::from("b"), EndReason::Absolute)]);
        assert_eq!(*rx_abs.borrow(), Some(EndReason::Absolute));
        assert!(store.lookup(&live_id, T0 + HOUR).is_some());
    }

    /// @test A restart keeps a live session: a store reopened over the
    /// same database finds it by the same cookie with the same user
    /// and deadlines, including a keep-alive taken before the restart.
    /// Sessions that ended while the server was down are not loaded
    /// and their rows are removed.
    #[test]
    fn reopen_restores_live_sessions_and_drops_ended_ones() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir, policy(30, 10), T0);
        let (id, _) = store.create("ops", T0).unwrap();
        let before = store.refresh(&key_of(&id), T0 + 20 * MIN).unwrap().unwrap();
        let (stale_id, _) = store.create("ops", T0).unwrap();
        drop(store);

        let reopened = open_store(&dir, policy(30, 10), T0 + 45 * MIN);
        let after = reopened.lookup(&id, T0 + 45 * MIN).unwrap();
        assert_eq!(after, before);
        assert!(reopened.lookup(&stale_id, T0 + 45 * MIN).is_none());
        let rows = reopened.db.load_sessions().unwrap();
        assert_eq!(rows.len(), 1, "the ended session's row is gone");
        assert_eq!(rows[0].key, key_of(&id));
    }

    /// @test At rest the database holds only a SHA-256 of the session
    /// id: the row carries the hash, and neither the database file nor
    /// its write-ahead log contains the id's text anywhere.
    #[test]
    fn database_holds_a_hash_never_the_raw_id() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir, policy(30, 10), T0);
        let (id, s) = store.create("ops", T0).unwrap();
        assert_eq!(id.len(), ID_BYTES * 2);
        assert_eq!(s.key, hex::encode(Sha256::digest(id.as_bytes())));
        assert_ne!(s.key, id);

        let rows = store.db.load_sessions().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, s.key);
        for file in ["sessions.db", "sessions.db-wal"] {
            let bytes = std::fs::read(dir.path().join(file)).unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains(&id), "{file} contains the raw id");
        }
        let all = std::fs::read(dir.path().join("sessions.db-wal"))
            .unwrap_or_default()
            .into_iter()
            .chain(std::fs::read(dir.path().join("sessions.db")).unwrap())
            .collect::<Vec<u8>>();
        let needle = s.key.as_bytes();
        assert!(
            all.windows(needle.len()).any(|w| w == needle),
            "the hash itself is what got stored"
        );
    }

    /// @test Two sessions get distinct ids and keys, and an unknown or
    /// empty cookie value finds nothing.
    #[test]
    fn ids_are_distinct_and_unknown_values_find_nothing() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir, policy(30, 10), T0);
        let (a, sa) = store.create("ops", T0).unwrap();
        let (b, sb) = store.create("ops", T0).unwrap();
        assert_ne!(a, b);
        assert_ne!(sa.key, sb.key);
        assert!(store.lookup("", T0).is_none());
        assert!(store.lookup(&sa.key, T0).is_none(), "the hash is no cookie");
        assert!(store.lookup(&a.to_uppercase(), T0).is_none());
    }

    /// @test The cookie is set with exactly HttpOnly, SameSite=Strict,
    /// Path=/ and, unless turned off, Secure; clearing it uses the same
    /// attributes with Max-Age=0.
    #[test]
    fn cookie_attributes_are_fixed() {
        assert_eq!(
            set_cookie("abc", true),
            "zenith_session=abc; Path=/; HttpOnly; SameSite=Strict; Secure"
        );
        assert_eq!(
            set_cookie("abc", false),
            "zenith_session=abc; Path=/; HttpOnly; SameSite=Strict"
        );
        assert_eq!(
            clear_cookie(true),
            "zenith_session=; Max-Age=0; Path=/; HttpOnly; SameSite=Strict; Secure"
        );
        assert_eq!(
            clear_cookie(false),
            "zenith_session=; Max-Age=0; Path=/; HttpOnly; SameSite=Strict"
        );
    }

    /// @test The session id is read from among other cookies and from
    /// any of several Cookie headers; look-alike names, an empty value
    /// and a missing cookie yield nothing.
    #[test]
    fn cookie_value_is_parsed_from_cookie_headers() {
        let h = headers(&[("cookie", "a=1; zenith_session=deadbeef ; b=2")]);
        assert_eq!(cookie_value(&h), Some("deadbeef"));
        let h = headers(&[("cookie", "a=1"), ("cookie", "zenith_session=cafe")]);
        assert_eq!(cookie_value(&h), Some("cafe"));
        let h = headers(&[(
            "cookie",
            "zenith_session_x=1; xzenith_session=2; zenith_session=",
        )]);
        assert_eq!(cookie_value(&h), None);
        assert_eq!(cookie_value(&HeaderMap::new()), None);
    }

    /// @test GET, HEAD and OPTIONS pass without any origin evidence;
    /// every other method (TRACE included) and a socket upgrade pass on
    /// Sec-Fetch-Site: same-origin and on nothing else that header says.
    #[test]
    fn same_origin_rule_trusts_sec_fetch_site_when_present() {
        let none = HeaderMap::new();
        for m in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(same_origin_allows(&m, &none), "{m}");
        }
        let same = headers(&[("sec-fetch-site", "same-origin")]);
        for m in [
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::PATCH,
            Method::TRACE,
        ] {
            assert!(same_origin_allows(&m, &same), "{m}");
            assert!(!same_origin_allows(&m, &none), "{m} with no evidence");
        }
        for site in ["same-site", "cross-site", "none"] {
            let h = headers(&[
                ("sec-fetch-site", site),
                ("origin", "http://ops:8090"),
                ("host", "ops:8090"),
            ]);
            assert!(!same_origin_allows(&Method::POST, &h), "{site}");
        }
        let upgrade = headers(&[("upgrade", "websocket"), ("sec-fetch-site", "cross-site")]);
        assert!(!same_origin_allows(&Method::GET, &upgrade));
        let upgrade = headers(&[("upgrade", "websocket"), ("sec-fetch-site", "same-origin")]);
        assert!(same_origin_allows(&Method::GET, &upgrade));
    }

    /// @test Without Sec-Fetch-Site, an unsafe request or an upgrade
    /// passes only when Origin's host and port equal Host (letter case
    /// aside); another host or port, an opaque "null" origin, or a
    /// missing Origin or Host is refused.
    #[test]
    fn same_origin_rule_falls_back_to_origin_against_host() {
        let ok = headers(&[
            ("origin", "http://Ops.Local:8090"),
            ("host", "ops.local:8090"),
        ]);
        assert!(same_origin_allows(&Method::POST, &ok));
        let tls = headers(&[("origin", "https://ops.local"), ("host", "ops.local")]);
        assert!(same_origin_allows(&Method::DELETE, &tls));
        for (origin, host) in [
            ("http://evil.example:8090", "ops.local:8090"),
            ("http://ops.local:8091", "ops.local:8090"),
            ("http://ops.local", "ops.local:8090"),
            ("null", "ops.local:8090"),
        ] {
            let h = headers(&[("origin", origin), ("host", host)]);
            assert!(!same_origin_allows(&Method::POST, &h), "{origin} vs {host}");
        }
        assert!(!same_origin_allows(
            &Method::POST,
            &headers(&[("host", "ops.local:8090")])
        ));
        assert!(!same_origin_allows(
            &Method::POST,
            &headers(&[("origin", "http://ops.local:8090")])
        ));
        let ws_ok = headers(&[
            ("upgrade", "WebSocket"),
            ("origin", "http://ops.local:8090"),
            ("host", "ops.local:8090"),
        ]);
        assert!(same_origin_allows(&Method::GET, &ws_ok));
        let ws_bad = headers(&[
            ("upgrade", "websocket"),
            ("origin", "http://evil.example"),
            ("host", "ops.local:8090"),
        ]);
        assert!(!same_origin_allows(&Method::GET, &ws_bad));
    }

    /// @test The session body has one shape: exactly auth_enabled false
    /// with auth off; user, both deadlines (idle null without an idle
    /// limit) and the server clock for a live session.
    #[test]
    fn status_body_shape() {
        assert_eq!(
            serde_json::to_value(SessionStatus::disabled()).unwrap(),
            serde_json::json!({ "auth_enabled": false })
        );
        let s = Session {
            key: "k".into(),
            user: Arc::from("ops"),
            idle_deadline_ms: None,
            absolute_deadline_ms: T0 + HOUR,
        };
        assert_eq!(
            serde_json::to_value(SessionStatus::live(&s, T0)).unwrap(),
            serde_json::json!({
                "auth_enabled": true,
                "user": "ops",
                "idle_deadline_ms": null,
                "absolute_deadline_ms": T0 + HOUR,
                "server_time_ms": T0,
            })
        );
    }

    /// @test The configured limits convert to milliseconds, with an
    /// idle limit of 0 meaning none.
    #[test]
    fn policy_from_config() {
        let mut auth = AuthSection::default();
        assert_eq!(
            SessionPolicy::from_config(&auth),
            SessionPolicy {
                idle_ms: 30 * MIN,
                absolute_ms: 10 * HOUR
            }
        );
        auth.session_idle_min = 0;
        auth.session_max_hours = 2;
        let p = SessionPolicy::from_config(&auth);
        assert_eq!(p.idle_deadline(T0, T0), None);
        assert_eq!(p.absolute_deadline(T0), T0 + 2 * HOUR);
    }
}
