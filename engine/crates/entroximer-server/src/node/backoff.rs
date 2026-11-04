//! Waiting out rate limiting: after a source refuses a background load for too many requests
//! (or with a bot check, which means the same for YouTube), background loads stop for a while,
//! twice as long each time it happens again. Rate limits apply to the engine's address, so the
//! wait is engine-wide.

use std::sync::Mutex;
use std::time::{Duration, Instant};

const FIRST_WAIT: Duration = Duration::from_secs(30);
const LONGEST_WAIT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug)]
pub(crate) struct Backoff(Mutex<State>);

#[derive(Debug)]
struct State {
    until: Option<Instant>,
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self(Mutex::new(State {
            until: None,
            next: FIRST_WAIT,
        }))
    }
}

impl Backoff {
    /// Whether background loads wait at `now`.
    pub(crate) fn is_waiting(&self, now: Instant) -> bool {
        self.state().until.is_some_and(|until| now < until)
    }

    /// A load was refused at `now`: background loads wait, longer than the last time. Returns
    /// how long.
    pub(crate) fn refused(&self, now: Instant) -> Duration {
        let mut state = self.state();
        let wait = state.next;
        state.until = Some(now + wait);
        state.next = (wait * 2).min(LONGEST_WAIT);
        wait
    }

    /// A load got through: the next refusal waits the shortest time again. A wait already on
    /// runs out as it was.
    pub(crate) fn got_through(&self) {
        self.state().next = FIRST_WAIT;
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_longer_each_time_up_to_a_limit() {
        let backoff = Backoff::default();
        let now = Instant::now();
        assert!(!backoff.is_waiting(now));
        assert_eq!(backoff.refused(now), FIRST_WAIT);
        assert!(backoff.is_waiting(now + FIRST_WAIT - Duration::from_secs(1)));
        assert!(!backoff.is_waiting(now + FIRST_WAIT));
        assert_eq!(backoff.refused(now), FIRST_WAIT * 2);
        for _ in 0..10 {
            backoff.refused(now);
        }
        assert_eq!(backoff.refused(now), LONGEST_WAIT);

        backoff.got_through();
        assert!(backoff.is_waiting(now), "the wait on runs out as it was");
        assert_eq!(backoff.refused(now), FIRST_WAIT);
    }
}
