use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Session {
    pub provider: String,
    pub model: Option<String>,
    /// Idle timeout: each successful lookup pushes `expires_at` this far into the future.
    pub idle_ttl: Duration,
    pub expires_at: Instant,
}

#[derive(Debug, Clone, Default)]
pub struct TokenStore {
    sessions: Arc<Mutex<HashMap<String, Session>>>,
}

impl TokenStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `idle_ttl` is a sliding window: the token lives as long as it keeps being used, and
    /// only expires after `idle_ttl` without a lookup (a backstop for a crashed `coder run`
    /// that never revoked it).
    ///
    /// # Panics
    ///
    /// Panics if the internal session lock is poisoned.
    #[must_use]
    pub fn mint(&self, provider: String, model: Option<String>, idle_ttl: Duration) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        let session = Session {
            provider,
            model,
            idle_ttl,
            expires_at: Instant::now() + idle_ttl,
        };
        self.sessions.lock().unwrap().insert(token.clone(), session);
        token
    }

    /// # Panics
    ///
    /// Panics if the internal session lock is poisoned.
    #[must_use]
    pub fn lookup(&self, token: &str) -> Option<Session> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions.get_mut(token)?;
        let now = Instant::now();
        if now < session.expires_at {
            session.expires_at = now + session.idle_ttl;
            Some(session.clone())
        } else {
            sessions.remove(token);
            None
        }
    }

    /// # Panics
    ///
    /// Panics if the internal session lock is poisoned.
    pub fn revoke(&self, token: &str) {
        self.sessions.lock().unwrap().remove(token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_token_is_lookupable() {
        let store = TokenStore::new();
        let token = store.mint("anthropic".into(), None, Duration::from_secs(60));
        let session = store.lookup(&token).unwrap();
        assert_eq!(session.provider, "anthropic");
    }

    #[test]
    fn revoked_token_is_not_lookupable() {
        let store = TokenStore::new();
        let token = store.mint("anthropic".into(), None, Duration::from_secs(60));
        store.revoke(&token);
        assert!(store.lookup(&token).is_none());
    }

    #[test]
    fn expired_token_is_not_lookupable() {
        let store = TokenStore::new();
        // wide margin (TTL vs. sleep) so scheduler jitter on a loaded runner can't flip the result
        let token = store.mint("anthropic".into(), None, Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(200));
        assert!(store.lookup(&token).is_none());
    }

    #[test]
    fn lookup_slides_the_expiry_window() {
        let store = TokenStore::new();
        let token = store.mint("anthropic".into(), None, Duration::from_millis(400));
        // used every 150ms for 600ms total: well inside the 400ms sliding window each time
        for _ in 0..4 {
            std::thread::sleep(Duration::from_millis(150));
            assert!(store.lookup(&token).is_some(), "active token expired");
        }
        std::thread::sleep(Duration::from_millis(600));
        assert!(store.lookup(&token).is_none(), "idle token did not expire");
    }

    #[test]
    fn unknown_token_returns_none() {
        let store = TokenStore::new();
        assert!(store.lookup("not-a-real-token").is_none());
    }
}
