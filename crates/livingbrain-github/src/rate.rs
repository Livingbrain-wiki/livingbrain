//! Rate-limit reading and the backoff decision. Pure: the caller passes the
//! response status, the headers and the current time, and gets back what to
//! do next. The driver sleeps (or re-queues the tick) for the seconds it is
//! told; nothing here waits.
//!
//! GitHub signals limits in two ways. The primary limit is a per-hour quota
//! reported in `x-ratelimit-remaining` and `x-ratelimit-reset` (epoch
//! seconds). The secondary limit is a burst or abuse guard that answers with
//! a `retry-after` in seconds, or with a 403 or 429 and no quota headers at
//! all. Both are read here, so the planner's budget is a ceiling, not the
//! only protection.

/// Below this many remaining primary-quota requests, wait for the reset
/// rather than spend the last of the quota on a backfill.
pub const LOW_WATER_MARK: u64 = 10;
/// How long to wait when a limit is signalled without a usable reset time.
pub const RATE_LIMIT_FALLBACK_SECS: u64 = 60;
/// How long to wait after a 5xx before the same request is retried.
pub const SERVER_ERROR_SECS: u64 = 30;

/// The limit headers of one response, read case-insensitively.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RateLimit {
    pub remaining: Option<u64>,
    /// Epoch seconds at which the primary quota resets.
    pub reset_at: Option<u64>,
    /// Seconds to wait, from a secondary limit.
    pub retry_after: Option<u64>,
}

/// Reads `x-ratelimit-remaining`, `x-ratelimit-reset` and `retry-after` from
/// a response's headers. Unparseable values read as absent.
#[must_use]
pub fn parse_rate_limit(headers: &[(&str, &str)]) -> RateLimit {
    let find = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .and_then(|(_, value)| value.trim().parse::<u64>().ok())
    };
    RateLimit {
        remaining: find("x-ratelimit-remaining"),
        reset_at: find("x-ratelimit-reset"),
        retry_after: find("retry-after"),
    }
}

/// What the driver does after a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backoff {
    /// Carry on with the next request.
    Proceed,
    /// Do not make another call for this many seconds.
    Wait { seconds: u64 },
    /// Not a limit: a permission, a missing resource, a bad request. Stop
    /// this item or run and surface it; retrying the same call will not help.
    Fail,
}

/// Decides the next step from a response's status, its limit headers and the
/// current epoch time.
///
/// In order: a `retry-after` is obeyed; an exhausted primary quota waits for
/// its reset; a 2xx or 3xx that leaves fewer than [`LOW_WATER_MARK`] requests
/// waits for the reset too, keeping a reserve; a 429 waits the fallback; a 5xx
/// waits [`SERVER_ERROR_SECS`]; every other status is a [`Backoff::Fail`].
#[must_use]
pub fn decide(status: u16, rate: RateLimit, now: u64) -> Backoff {
    let until_reset = |fallback: u64| match rate.reset_at {
        Some(reset) => reset.saturating_sub(now).saturating_add(1),
        None => fallback,
    };
    if let Some(seconds) = rate.retry_after {
        return Backoff::Wait {
            seconds: seconds.max(1),
        };
    }
    if rate.remaining == Some(0) {
        return Backoff::Wait {
            seconds: until_reset(RATE_LIMIT_FALLBACK_SECS),
        };
    }
    match status {
        200..=399 => match rate.remaining {
            Some(remaining) if remaining < LOW_WATER_MARK => Backoff::Wait {
                seconds: until_reset(RATE_LIMIT_FALLBACK_SECS),
            },
            _ => Backoff::Proceed,
        },
        429 => Backoff::Wait {
            seconds: RATE_LIMIT_FALLBACK_SECS,
        },
        500..=599 => Backoff::Wait {
            seconds: SERVER_ERROR_SECS,
        },
        _ => Backoff::Fail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000;

    #[test]
    fn headers_are_read_case_insensitively_and_bad_values_are_absent() {
        let rate = parse_rate_limit(&[
            ("X-RateLimit-Remaining", "4000"),
            ("X-RateLimit-Reset", " 1700000600 "),
            ("Retry-After", "not-a-number"),
        ]);
        assert_eq!(
            rate,
            RateLimit {
                remaining: Some(4000),
                reset_at: Some(1_700_000_600),
                retry_after: None,
            }
        );
    }

    #[test]
    fn healthy_success_proceeds() {
        let rate = RateLimit {
            remaining: Some(4000),
            reset_at: Some(NOW + 600),
            retry_after: None,
        };
        assert_eq!(decide(200, rate, NOW), Backoff::Proceed);
        assert_eq!(decide(200, RateLimit::default(), NOW), Backoff::Proceed);
    }

    #[test]
    fn retry_after_is_obeyed_and_never_zero() {
        let rate = RateLimit {
            retry_after: Some(0),
            ..RateLimit::default()
        };
        assert_eq!(decide(403, rate, NOW), Backoff::Wait { seconds: 1 });
        let rate = RateLimit {
            retry_after: Some(120),
            ..RateLimit::default()
        };
        assert_eq!(decide(429, rate, NOW), Backoff::Wait { seconds: 120 });
    }

    #[test]
    fn an_exhausted_quota_waits_for_its_reset() {
        let rate = RateLimit {
            remaining: Some(0),
            reset_at: Some(NOW + 300),
            retry_after: None,
        };
        assert_eq!(decide(403, rate, NOW), Backoff::Wait { seconds: 301 });
        assert_eq!(decide(200, rate, NOW), Backoff::Wait { seconds: 301 });
        let no_reset = RateLimit {
            remaining: Some(0),
            ..RateLimit::default()
        };
        assert_eq!(
            decide(403, no_reset, NOW),
            Backoff::Wait {
                seconds: RATE_LIMIT_FALLBACK_SECS
            }
        );
    }

    #[test]
    fn a_reset_already_past_waits_one_second_not_a_negative() {
        let rate = RateLimit {
            remaining: Some(0),
            reset_at: Some(NOW - 50),
            retry_after: None,
        };
        assert_eq!(decide(403, rate, NOW), Backoff::Wait { seconds: 1 });
    }

    #[test]
    fn a_low_reserve_waits_for_the_reset_to_keep_the_last_requests() {
        let rate = RateLimit {
            remaining: Some(LOW_WATER_MARK - 1),
            reset_at: Some(NOW + 90),
            retry_after: None,
        };
        assert_eq!(decide(200, rate, NOW), Backoff::Wait { seconds: 91 });
    }

    #[test]
    fn secondary_429_waits_the_fallback_and_5xx_waits_briefly() {
        assert_eq!(
            decide(429, RateLimit::default(), NOW),
            Backoff::Wait {
                seconds: RATE_LIMIT_FALLBACK_SECS
            }
        );
        assert_eq!(
            decide(502, RateLimit::default(), NOW),
            Backoff::Wait {
                seconds: SERVER_ERROR_SECS
            }
        );
    }

    #[test]
    fn a_403_without_a_limit_is_a_permission_failure_not_a_wait() {
        let healthy = RateLimit {
            remaining: Some(4000),
            reset_at: Some(NOW + 600),
            retry_after: None,
        };
        assert_eq!(decide(403, healthy, NOW), Backoff::Fail);
        assert_eq!(decide(403, RateLimit::default(), NOW), Backoff::Fail);
        assert_eq!(decide(404, RateLimit::default(), NOW), Backoff::Fail);
        assert_eq!(decide(401, RateLimit::default(), NOW), Backoff::Fail);
    }
}
