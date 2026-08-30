//! What one person is allowed to spend.
//!
//! Every request here costs money at a provider, and an agent run costs several:
//! the loop is bounded at twelve model turns, each of which may call tools whose
//! output comes back as more input. Step count was the only ceiling, and step
//! count is not a budget -- one step over a large file is worth more than ten
//! over a small one.
//!
//! This is a rolling window per user, counting requests and, where a provider
//! tells us, tokens. It is off by default: a single-user deployment paying its
//! own bill does not need to be told to stop, and turning it on for everyone
//! would be a behaviour change nobody asked for.
//!
//! Fixed windows, not a token bucket. A bucket smooths bursts, which is the
//! right shape for protecting a service from load; this is protecting a bill
//! from a runaway loop, where what matters is the total over an hour and a burst
//! inside it is fine.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

/// Users whose windows are remembered. Beyond this the coldest are dropped,
/// which only ever forgives spending -- it never invents any.
const MAX_TRACKED_USERS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Requests per window. `0` means unlimited.
    pub requests: u32,
    /// Tokens per window, counted only where a provider reports usage. `0`
    /// means unlimited.
    pub tokens: u64,
    pub window: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            requests: 0,
            tokens: 0,
            window: Duration::from_secs(3600),
        }
    }
}

impl Limits {
    pub fn enabled(&self) -> bool {
        self.requests > 0 || self.tokens > 0
    }
}

/// Why a request was refused, and when to try again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exceeded {
    pub kind: Kind,
    pub limit: u64,
    pub used: u64,
    pub retry_after: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Requests,
    Tokens,
}

impl Exceeded {
    pub fn message(&self) -> String {
        let seconds = self.retry_after.as_secs().max(1);
        match self.kind {
            Kind::Requests => format!(
                "you have used {} of {} requests in this window; try again in {seconds} seconds",
                self.used, self.limit
            ),
            Kind::Tokens => format!(
                "you have used {} of {} tokens in this window; try again in {seconds} seconds",
                self.used, self.limit
            ),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Window {
    started: Instant,
    requests: u32,
    tokens: u64,
}

impl Window {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            requests: 0,
            tokens: 0,
        }
    }

    fn expired(&self, now: Instant, window: Duration) -> bool {
        now.duration_since(self.started) >= window
    }

    fn remaining(&self, now: Instant, window: Duration) -> Duration {
        window.saturating_sub(now.duration_since(self.started))
    }
}

#[derive(Debug)]
pub struct Quotas {
    limits: Limits,
    windows: Mutex<HashMap<String, Window>>,
}

impl Quotas {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            windows: Mutex::new(HashMap::new()),
        }
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Count one request against this user, or refuse it.
    ///
    /// Tokens are checked here but charged later, when a provider says what a
    /// run actually cost: a user who has already blown the token budget is
    /// stopped at the door, and one who is under it is let through even if this
    /// request will take them over. Charging in advance would mean guessing, and
    /// a guess that is too high refuses work that would have been affordable.
    pub fn admit(&self, user: &str) -> Result<(), Exceeded> {
        if !self.limits.enabled() {
            return Ok(());
        }
        let now = Instant::now();
        let mut windows = self.lock();
        prune(&mut windows, now, self.limits.window);

        let window = windows
            .entry(user.to_owned())
            .and_modify(|window| {
                if window.expired(now, self.limits.window) {
                    *window = Window::new(now);
                }
            })
            .or_insert_with(|| Window::new(now));

        if self.limits.tokens > 0 && window.tokens >= self.limits.tokens {
            return Err(Exceeded {
                kind: Kind::Tokens,
                limit: self.limits.tokens,
                used: window.tokens,
                retry_after: window.remaining(now, self.limits.window),
            });
        }
        if self.limits.requests > 0 && window.requests >= self.limits.requests {
            return Err(Exceeded {
                kind: Kind::Requests,
                limit: self.limits.requests as u64,
                used: window.requests as u64,
                retry_after: window.remaining(now, self.limits.window),
            });
        }

        window.requests += 1;
        Ok(())
    }

    /// Charge tokens a provider reported. A run that reports nothing charges
    /// nothing, which is the honest accounting -- inventing a number would make
    /// the quota a fiction.
    pub fn charge(&self, user: &str, tokens: u64) {
        if tokens == 0 || !self.limits.enabled() {
            return;
        }
        let now = Instant::now();
        let mut windows = self.lock();
        let window = windows
            .entry(user.to_owned())
            .and_modify(|window| {
                if window.expired(now, self.limits.window) {
                    *window = Window::new(now);
                }
            })
            .or_insert_with(|| Window::new(now));
        window.tokens = window.tokens.saturating_add(tokens);
    }

    /// What this user has spent in the current window, for the client to show.
    pub fn usage(&self, user: &str) -> (u32, u64) {
        let now = Instant::now();
        let windows = self.lock();
        windows
            .get(user)
            .filter(|window| !window.expired(now, self.limits.window))
            .map_or((0, 0), |window| (window.requests, window.tokens))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Window>> {
        self.windows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Drop windows that have run out, and if that is not enough, the oldest.
///
/// Forgetting a window can only forgive spending, never invent it, so the cap is
/// a memory bound rather than a correctness one.
fn prune(windows: &mut HashMap<String, Window>, now: Instant, window: Duration) {
    windows.retain(|_, existing| !existing.expired(now, window));
    if windows.len() <= MAX_TRACKED_USERS {
        return;
    }
    let mut by_age: Vec<(String, Instant)> = windows
        .iter()
        .map(|(user, window)| (user.clone(), window.started))
        .collect();
    by_age.sort_by_key(|(_, started)| *started);
    for (user, _) in by_age.into_iter().take(windows.len() - MAX_TRACKED_USERS) {
        windows.remove(&user);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(requests: u32, tokens: u64) -> Limits {
        Limits {
            requests,
            tokens,
            window: Duration::from_secs(3600),
        }
    }

    #[test]
    fn nothing_is_limited_until_something_is_configured() {
        let quotas = Quotas::new(Limits::default());
        for _ in 0..1_000 {
            assert!(quotas.admit("riley").is_ok());
        }
        assert_eq!(
            quotas.usage("riley"),
            (0, 0),
            "an unlimited deployment counts nothing"
        );
    }

    #[test]
    fn a_request_budget_runs_out() {
        let quotas = Quotas::new(limits(3, 0));
        for _ in 0..3 {
            assert!(quotas.admit("riley").is_ok());
        }

        let refused = quotas.admit("riley").unwrap_err();
        assert_eq!(refused.kind, Kind::Requests);
        assert_eq!(refused.limit, 3);
        assert_eq!(refused.used, 3);
        assert!(refused.retry_after <= Duration::from_secs(3600));
        assert!(refused.message().contains("3 of 3 requests"));
    }

    /// One person spending everything must not stop anybody else.
    #[test]
    fn one_users_budget_is_not_anothers() {
        let quotas = Quotas::new(limits(2, 0));
        assert!(quotas.admit("riley").is_ok());
        assert!(quotas.admit("riley").is_ok());
        assert!(quotas.admit("riley").is_err());

        assert!(
            quotas.admit("sam").is_ok(),
            "sam was charged for riley's spending"
        );
        assert_eq!(quotas.usage("sam"), (1, 0));
    }

    #[test]
    fn a_token_budget_stops_the_next_request_once_it_is_spent() {
        let quotas = Quotas::new(limits(0, 1_000));

        assert!(quotas.admit("riley").is_ok());
        quotas.charge("riley", 400);
        assert!(quotas.admit("riley").is_ok(), "still under budget");

        // The request that takes them over is allowed to finish; the next is not.
        quotas.charge("riley", 900);
        let refused = quotas.admit("riley").unwrap_err();
        assert_eq!(refused.kind, Kind::Tokens);
        assert_eq!(refused.used, 1_300);
        assert!(refused.message().contains("1300 of 1000 tokens"));
    }

    #[test]
    fn a_run_that_reports_no_usage_is_charged_nothing() {
        let quotas = Quotas::new(limits(0, 100));
        quotas.charge("riley", 0);
        assert_eq!(quotas.usage("riley"), (0, 0));
        assert!(quotas.admit("riley").is_ok());
    }

    #[test]
    fn a_window_that_has_run_out_starts_again() {
        let quotas = Quotas::new(Limits {
            requests: 1,
            tokens: 0,
            window: Duration::from_millis(60),
        });

        assert!(quotas.admit("riley").is_ok());
        assert!(quotas.admit("riley").is_err());

        std::thread::sleep(Duration::from_millis(80));
        assert!(quotas.admit("riley").is_ok(), "the window never reset");
        assert_eq!(quotas.usage("riley").0, 1);
    }

    #[test]
    fn usage_reports_what_has_been_spent_in_this_window() {
        let quotas = Quotas::new(limits(10, 1_000));
        quotas.admit("riley").unwrap();
        quotas.admit("riley").unwrap();
        quotas.charge("riley", 250);

        assert_eq!(quotas.usage("riley"), (2, 250));
        assert_eq!(quotas.usage("nobody"), (0, 0));
    }

    #[test]
    fn charging_a_user_who_has_never_asked_for_anything_still_counts() {
        let quotas = Quotas::new(limits(0, 100));
        quotas.charge("riley", 50);
        assert_eq!(quotas.usage("riley"), (0, 50));
    }

    #[test]
    fn expired_windows_are_not_kept_forever() {
        let quotas = Quotas::new(Limits {
            requests: 5,
            tokens: 0,
            window: Duration::from_millis(30),
        });
        for index in 0..50 {
            quotas.admit(&format!("user{index}")).unwrap();
        }
        std::thread::sleep(Duration::from_millis(50));

        // Admitting anyone prunes what has run out.
        quotas.admit("someone-else").unwrap();
        assert_eq!(quotas.lock().len(), 1);
    }

    #[test]
    fn a_token_only_budget_does_not_limit_requests() {
        let quotas = Quotas::new(limits(0, 1_000));
        for _ in 0..100 {
            assert!(quotas.admit("riley").is_ok());
        }
    }
}
