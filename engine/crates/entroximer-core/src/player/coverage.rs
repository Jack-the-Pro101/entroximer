//! Decides how to obtain media for a position the timeline does not have yet.
//!
//! - Positions just past a download's frontier are waited for when the download will get there
//!   within `wait_threshold`, judged by its observed speed, or when that download was itself
//!   opened for the position: sources often start at the keyframe or cluster before the
//!   requested time, and re-opening would only land in the same place again.
//! - Anything farther away is re-opened at that position when the source can seek by time.
//! - A source that cannot seek by time is re-opened from the beginning only when nothing else
//!   will reach the position; already buffered packets are skipped as it catches up.

use std::time::Duration;

use crate::producer::ProducerCaps;
use crate::timeline::{Pending, Ticks, duration, ticks};

#[derive(Clone, Debug)]
pub struct CoverageConfig {
    /// Longest expected wait for a download to reach a position before re-opening there instead.
    pub wait_threshold: Duration,
    /// Re-opened producers start this far before the position, so decoders can preroll.
    pub reopen_lead: Duration,
    /// How far ahead of an interrupted range's end a producer is opened to fill the gap after it.
    pub prefetch: Duration,
}

impl Default for CoverageConfig {
    fn default() -> Self {
        Self {
            wait_threshold: Duration::from_secs(2),
            reopen_lead: Duration::from_millis(500),
            prefetch: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coverage {
    /// Media is on its way; output silence meanwhile.
    Wait,
    /// Open a producer at `at` (`None` = the beginning). `exclusive` cancels all other
    /// producers first, keeping one download at a time for sources that can seek by time.
    Open { at: Option<Ticks>, exclusive: bool },
}

/// `rate` is the frontier producer's speed in media seconds per second, once measurable.
pub(super) fn plan(
    pending: Pending,
    t: Ticks,
    caps: ProducerCaps,
    rate: Option<f64>,
    cfg: &CoverageConfig,
) -> Coverage {
    let reopen_at = (t - ticks(cfg.reopen_lead)).max(0);
    let reopen = || Coverage::Open {
        at: Some(reopen_at),
        exclusive: true,
    };
    match pending {
        Pending::Opening { .. } => Coverage::Wait,
        Pending::Frontier {
            opened_at,
            frontier,
            ..
        } if caps.time_seek => {
            if opened_at >= reopen_at {
                return Coverage::Wait;
            }
            let missing = duration(t - frontier).as_secs_f64();
            // a producer that only just started is assumed to download in real time
            let eta = missing / rate.unwrap_or(1.0);
            if eta <= cfg.wait_threshold.as_secs_f64() {
                Coverage::Wait
            } else {
                reopen()
            }
        }
        Pending::Frontier { .. } => Coverage::Wait,
        Pending::Gap if caps.time_seek => reopen(),
        Pending::Gap => Coverage::Open {
            at: None,
            exclusive: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::TICKS_PER_SECOND;

    const SEEKABLE: ProducerCaps = ProducerCaps {
        time_seek: true,
        live: false,
    };
    const PIPE: ProducerCaps = ProducerCaps {
        time_seek: false,
        live: false,
    };

    fn frontier_at(seconds: i64) -> Pending {
        Pending::Frontier {
            producer: 1,
            opened_at: 0,
            frontier: seconds * TICKS_PER_SECOND,
        }
    }

    fn plan_at(pending: Pending, seconds: i64, caps: ProducerCaps, rate: Option<f64>) -> Coverage {
        plan(
            pending,
            seconds * TICKS_PER_SECOND,
            caps,
            rate,
            &CoverageConfig::default(),
        )
    }

    #[test]
    fn waits_for_a_fast_download() {
        // 20 s missing at 15x real time arrives in ~1.3 s
        assert_eq!(
            plan_at(frontier_at(40), 60, SEEKABLE, Some(15.0)),
            Coverage::Wait
        );
    }

    #[test]
    fn reopens_far_ahead_of_a_slow_download() {
        assert_eq!(
            plan_at(frontier_at(40), 600, SEEKABLE, Some(15.0)),
            Coverage::Open {
                at: Some(600 * TICKS_PER_SECOND - TICKS_PER_SECOND / 2),
                exclusive: true
            }
        );
        // no rate yet: assume real time, so 5 s missing is too far
        assert!(matches!(
            plan_at(frontier_at(40), 45, SEEKABLE, None),
            Coverage::Open { .. }
        ));
    }

    #[test]
    fn never_reopens_where_the_frontier_producer_was_opened() {
        // opened for 40 s but started at the 35 s cluster: re-opening would start there again
        let pending = Pending::Frontier {
            producer: 2,
            opened_at: 40 * TICKS_PER_SECOND - TICKS_PER_SECOND / 2,
            frontier: 35 * TICKS_PER_SECOND,
        };
        assert_eq!(plan_at(pending, 40, SEEKABLE, Some(1.0)), Coverage::Wait);
    }

    #[test]
    fn pipes_wait_or_restart_from_the_beginning() {
        assert_eq!(
            plan_at(frontier_at(40), 600, PIPE, Some(0.5)),
            Coverage::Wait
        );
        assert_eq!(
            plan_at(Pending::Gap, 600, PIPE, None),
            Coverage::Open {
                at: None,
                exclusive: false
            }
        );
    }

    #[test]
    fn opening_producers_are_waited_for() {
        assert_eq!(
            plan_at(Pending::Opening { producer: 2 }, 600, SEEKABLE, None),
            Coverage::Wait
        );
    }
}
