//! Keeping the session alive by rotating its cookies, as a browser with YouTube open does.
//!
//! Google ties a signed-in session to short-lived `__Secure-*PSIDTS` cookies, which signed-in pages
//! have it replace from time to time; a session whose tokens go stale is ended sooner or later.
//! So every few hours, and as soon as the session is found expired, the engine visits YouTube
//! with the session in headless Chromium and keeps the cookies YouTube sets. For this the engine
//! must be the session's only user: two browsers rotating one session look like a stolen session
//! to Google.
//!
//! A failed rotation only counts against the session when YouTube itself refuses it. That means
//! the page comes back signed out, or Google rejects the rotation. After
//! `ENTROXIMER_YOUTUBE_ROTATE_MAX_REFUSALS` refusals in a row, rotation gives up, and the session
//! stays expired until fresh cookies are imported. Failures on the engine's side (no network, the
//! browser crashed) are retried, waiting longer each time, for as long as they last.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use entroximer_core::error::display_chain;
use reqwest::Url;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

use super::browser::{Browser, BrowserConfig, BrowserError, PageState, find_chromium};
use super::cookies::CookieJar;
use super::session::RotationState;
use super::{AuthPolicy, CheckOutcome, SessionError, SessionState, SessionStore, check_session};
use crate::config::MediaContext;

const START_URL: &str = "https://www.youtube.com/";
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// How long after the page loaded its scripts get to rotate the cookies. Pages whose tokens are
/// still fresh rotate nothing, and the visit ends after this.
const SETTLE: Duration = Duration::from_secs(15);
/// A whole visit, from starting the browser to reading its cookies.
const VISIT_TIMEOUT: Duration = Duration::from_secs(90);

/// Whether the page is signed in, by the flag YouTube's own scripts read, and where it ended up.
const SIGNED_IN_SCRIPT: &str = "(() => ({
    loggedIn: typeof ytcfg === 'object' && typeof ytcfg.get === 'function'
        ? (ytcfg.get('LOGGED_IN') ?? null) : null,
    host: location.hostname,
}))()";

/// After a fresh import or the engine's start, a session never rotated is rotated this soon.
const FIRST_DELAY: Duration = Duration::from_secs(30);
/// Sessions imported by another process (e.g. `podman exec … import`) are noticed this soon.
const POLL: Duration = Duration::from_secs(10 * 60);
/// A session found expired is rotated right away, but not sooner than this after the last try:
/// an account that cannot pass the check would otherwise be rotated over and over.
const EXPIRY_SPACING: Duration = Duration::from_secs(30 * 60);
/// Waits after YouTube refused, the last repeated if more refusals are allowed.
const REFUSAL_WAITS: &[Duration] = &[Duration::from_secs(5 * 60), Duration::from_secs(30 * 60)];
/// The first wait after a failure on the engine's side; it doubles up to the interval.
const FIRST_RETRY: Duration = Duration::from_secs(60);
/// Inconclusive rotations in a row that are worth a warning (and again every so many), e.g.
/// because YouTube changed how its page says it is signed in.
const INCONCLUSIVE_WARNING: u32 = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RotationOutcome {
    NoSession,
    /// The page was signed in, and the cookies it set were kept. `tokens_changed` tells whether
    /// Google issued new tokens, which it does when the old ones are due.
    Rotated {
        tokens_changed: bool,
    },
    /// YouTube refused the session.
    SignedOut(String),
    /// Nothing could be told, e.g. the browser failed or YouTube was unreachable; the session
    /// is unchanged.
    Inconclusive(String),
}

/// A rotation, as recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RotationReport {
    pub outcome: RotationOutcome,
    /// Refusals in a row, this one included.
    pub refusals: u32,
    /// This refusal was the last allowed: the session is now expired.
    pub gave_up: bool,
    /// The session was expired before the rotation, which signed it back in.
    pub revived: bool,
}

impl RotationReport {
    fn new(outcome: RotationOutcome) -> Self {
        Self {
            outcome,
            refusals: 0,
            gave_up: false,
            revived: false,
        }
    }
}

/// The browser to rotate the session with, if one is found.
pub fn browser_config(ctx: &MediaContext) -> Option<BrowserConfig> {
    let program = find_chromium(ctx.config.chromium.as_deref())?;
    Some(BrowserConfig {
        program,
        no_sandbox: ctx.config.chromium_no_sandbox,
        proxy: ctx.youtube.proxy().cloned(),
    })
}

/// Rotates the session now and records the outcome. A session this brings back from expired
/// is checked again with yt-dlp.
pub async fn rotate_session(
    ctx: &MediaContext,
    browser: &BrowserConfig,
) -> Result<RotationReport, SessionError> {
    let start = Url::parse(START_URL).expect("valid URL");
    let report = rotate_from(ctx, browser, &start).await?;
    if report.revived {
        match check_session(ctx, true).await {
            Ok(CheckOutcome::Inconclusive(reason)) => {
                tracing::warn!(%reason, "could not check the rotated YouTube session");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(
                error = %display_chain(&e),
                "could not check the rotated YouTube session"
            ),
        }
    }
    Ok(report)
}

async fn rotate_from(
    ctx: &MediaContext,
    browser: &BrowserConfig,
    start: &Url,
) -> Result<RotationReport, SessionError> {
    let store = &ctx.youtube.store;
    let Some(_rotating) = store.try_lock_rotation()? else {
        return Ok(RotationReport::new(RotationOutcome::Inconclusive(
            "another rotation is running".to_owned(),
        )));
    };
    // an expired session too: a browser may bring it back
    let Some(copy) = store.checkout_any(ctx.work_dir())? else {
        return Ok(RotationReport::new(RotationOutcome::NoSession));
    };
    let generation = copy.generation();
    // YouTube has already signed the session out (yt-dlp's write-back merged that in): no
    // browser can sign it back in
    let outcome = if !copy.base().has_youtube_login() {
        RotationOutcome::SignedOut("YouTube has cleared the session's login cookies".to_owned())
    } else {
        match visit(browser, ctx.work_dir(), copy.base(), start).await {
            Ok(visit) => {
                let outcome = judge(copy.base(), &visit);
                // anything else leaves the copy as it was, so dropping it changes nothing
                if matches!(outcome, RotationOutcome::Rotated { .. }) {
                    copy.absorb(&visit.accepted, &visit.cookies)?;
                }
                outcome
            }
            Err(e) => RotationOutcome::Inconclusive(display_chain(&e)),
        }
    };
    // merges what the browser changed into the stored session
    drop(copy);
    Ok(record(
        store,
        generation,
        outcome,
        ctx.config.youtube.rotate_max_refusals,
    ))
}

/// What a visit to YouTube showed.
#[derive(Debug)]
struct Visit {
    /// The session's cookies the browser took.
    accepted: CookieJar,
    /// The browser's cookies after the visit.
    cookies: CookieJar,
    /// YouTube's own flag; `None` when the page did not have it.
    logged_in: Option<bool>,
    /// Where the page ended up.
    host: String,
    /// Statuses of Google's answers to cookie rotation requests.
    rotations: Vec<u16>,
}

#[derive(Debug, thiserror::Error)]
enum VisitError {
    #[error(transparent)]
    Browser(#[from] BrowserError),
    #[error("the visit took longer than {0:?}")]
    Timeout(Duration),
}

/// Visits `start` in a fresh browser holding `jar`'s cookies.
async fn visit(
    config: &BrowserConfig,
    work_dir: &Path,
    jar: &CookieJar,
    start: &Url,
) -> Result<Visit, VisitError> {
    let mut browser = Browser::launch(config, work_dir).await?;
    let visited = tokio::time::timeout(VISIT_TIMEOUT, visit_with(&mut browser, jar, start)).await;
    let visited = match visited {
        Ok(Err(VisitError::Browser(e))) => Err(VisitError::Browser(browser.explain(e))),
        Ok(visited) => visited,
        Err(_) => Err(VisitError::Timeout(VISIT_TIMEOUT)),
    };
    browser.close().await;
    visited
}

async fn visit_with(
    browser: &mut Browser,
    jar: &CookieJar,
    start: &Url,
) -> Result<Visit, VisitError> {
    let user_agent = browser.user_agent().await?;
    let accepted = browser.set_cookies(jar).await?;
    let mut page = browser.open_page(&user_agent).await?;
    page.navigate(start, LOAD_TIMEOUT).await?;
    page.wait_for_rotation(SETTLE).await;
    let shown = page.evaluate(SIGNED_IN_SCRIPT).await?;
    let PageState {
        rotations, blocked, ..
    } = page.state();
    tracing::debug!(?rotations, ?blocked, "visited YouTube");
    drop(page);
    Ok(Visit {
        accepted,
        cookies: browser.cookies().await?,
        logged_in: shown["loggedIn"].as_bool(),
        host: shown["host"].as_str().unwrap_or_default().to_owned(),
        rotations,
    })
}

/// What a visit says about the session. Whatever cannot be told for sure is inconclusive,
/// never a refusal.
fn judge(base: &CookieJar, visit: &Visit) -> RotationOutcome {
    // had the browser not taken the session's own cookies, a signed-out page would wrongly count
    // against the session
    if !visit.accepted.has_youtube_login() {
        return RotationOutcome::Inconclusive(
            "the browser did not take the session's login cookies".to_owned(),
        );
    }
    if visit.host == "accounts.google.com" {
        return RotationOutcome::SignedOut("YouTube sent the browser to sign in".to_owned());
    }
    if !visit.cookies.has_youtube_login() {
        return RotationOutcome::SignedOut(
            "YouTube cleared the session's login cookies".to_owned(),
        );
    }
    // 429 only says the tokens were rotated moments ago
    let succeeded = visit.rotations.iter().any(|s| (200..300).contains(s));
    if let Some(status) = visit
        .rotations
        .iter()
        .find(|s| (400..500).contains(*s) && **s != 429)
        && !succeeded
    {
        return RotationOutcome::SignedOut(format!(
            "Google refused to rotate the cookies (HTTP {status})"
        ));
    }
    match visit.logged_in {
        Some(true) => RotationOutcome::Rotated {
            tokens_changed: base.rotating_tokens() != visit.cookies.rotating_tokens(),
        },
        Some(false) => RotationOutcome::SignedOut("YouTube's page is signed out".to_owned()),
        None => RotationOutcome::Inconclusive(format!(
            "the page at {} did not say whether it is signed in (ytcfg's LOGGED_IN is missing)",
            visit.host
        )),
    }
}

/// Records `outcome` for the session of `generation`, giving up after `max_refusals` refusals
/// in a row.
fn record(
    store: &SessionStore,
    generation: u64,
    outcome: RotationOutcome,
    max_refusals: u32,
) -> RotationReport {
    let mut report = RotationReport::new(outcome);
    let replaced = || tracing::debug!("the YouTube session was replaced while rotating it");
    let failed = |e: SessionError| {
        tracing::warn!(error = %display_chain(&e), "cannot record the YouTube session's rotation");
    };
    match &report.outcome {
        RotationOutcome::NoSession => {}
        RotationOutcome::Rotated { tokens_changed } => match store.rotated(generation) {
            Ok(Some(state)) => {
                report.revived = state == SessionState::Expired;
                tracing::info!(
                    tokens_changed,
                    revived = report.revived,
                    "rotated the YouTube session's cookies"
                );
            }
            Ok(None) => replaced(),
            Err(e) => failed(e),
        },
        RotationOutcome::SignedOut(reason) => match store.refused(generation) {
            Ok(Some(refusals)) => {
                report.refusals = refusals;
                report.gave_up = refusals >= max_refusals;
                if report.gave_up {
                    tracing::warn!(
                        %reason,
                        refusals,
                        "YouTube keeps refusing the session; no longer rotating it. Import fresh \
                         cookies with `entroximer youtube cookies import`"
                    );
                    if let Err(e) = store.record(generation, SessionState::Expired, true) {
                        failed(e);
                    }
                } else {
                    tracing::warn!(
                        %reason,
                        refusals,
                        max_refusals,
                        "YouTube refused to rotate the session; trying again later"
                    );
                }
            }
            Ok(None) => replaced(),
            Err(e) => failed(e),
        },
        // retried; only a run of these is worth a warning, see `Plan::attempted`
        RotationOutcome::Inconclusive(reason) => {
            tracing::info!(%reason, "could not rotate the YouTube session; trying again later");
        }
    }
    report
}

/// How often the session is rotated, and how many refusals end it.
#[derive(Clone, Copy, Debug)]
pub struct RotationSchedule {
    pub interval: Duration,
    pub max_refusals: u32,
}

/// Rotates the session every `schedule.interval`, sooner when it is found expired, and retries
/// failures. Runs forever.
pub async fn maintain_rotation(
    ctx: Arc<MediaContext>,
    browser: BrowserConfig,
    schedule: RotationSchedule,
) {
    debug_assert_ne!(ctx.youtube.policy, AuthPolicy::Never);
    let store = Arc::clone(&ctx.youtube.store);
    run_schedule(&store, schedule, || async {
        rotate_session(&ctx, &browser).await.unwrap_or_else(|e| {
            RotationReport::new(RotationOutcome::Inconclusive(display_chain(&e)))
        })
    })
    .await;
}

async fn run_schedule<F, Fut>(store: &SessionStore, schedule: RotationSchedule, mut rotate: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = RotationReport>,
{
    let mut changes = store.subscribe();
    let mut plan = Plan::new(None, Instant::now());
    loop {
        let now = Instant::now();
        let due = match store.rotation() {
            Ok(rotation) => plan.due(&rotation, schedule, now, unix_now()),
            Err(e) => {
                tracing::warn!(error = %display_chain(&e), "cannot read the YouTube session");
                None
            }
        };
        if due.is_some_and(|at| at <= now) {
            let report = rotate().await;
            plan.attempted(&report, schedule, Instant::now());
            if let RotationOutcome::Inconclusive(reason) = &report.outcome
                && plan.inconclusive.is_multiple_of(INCONCLUSIVE_WARNING)
            {
                tracing::warn!(
                    %reason,
                    in_a_row = plan.inconclusive,
                    "the YouTube session keeps failing to rotate; if this is not a network or \
                     browser problem, YouTube's page may have changed and the engine needs an update"
                );
            }
            continue;
        }
        let wake = due.map_or(now + POLL, |at| at.min(now + POLL));
        tokio::select! {
            () = tokio::time::sleep_until(wake) => {}
            change = changes.recv() => match change {
                Ok(SessionState::Expired) => plan.expired = true,
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            },
        }
    }
}

/// When to rotate next, for the session of one generation.
#[derive(Debug)]
struct Plan {
    generation: Option<u64>,
    /// When this session was first seen; one never rotated is rotated soon after.
    seen: Instant,
    /// The rotation time last read from the store, and when that makes the next one due. Kept,
    /// so that the deadline is worked out from the wall clock once per rotation.
    scheduled: Option<(Option<u64>, Instant)>,
    /// When to try again after a failure, instead of on schedule.
    retry_at: Option<Instant>,
    next_retry: Duration,
    last_attempt: Option<Instant>,
    /// The session expired since the last attempt.
    expired: bool,
    /// Inconclusive attempts in a row.
    inconclusive: u32,
}

impl Plan {
    fn new(generation: Option<u64>, now: Instant) -> Self {
        Self {
            generation,
            seen: now,
            scheduled: None,
            retry_at: None,
            next_retry: FIRST_RETRY,
            last_attempt: None,
            expired: false,
            inconclusive: 0,
        }
    }

    /// When to rotate; `None` while there is nothing to rotate, or rotation gave up.
    fn due(
        &mut self,
        rotation: &RotationState,
        schedule: RotationSchedule,
        now: Instant,
        unix_now: u64,
    ) -> Option<Instant> {
        if self.generation != Some(rotation.generation) {
            *self = Self::new(Some(rotation.generation), now);
            self.expired = rotation.state == SessionState::Expired;
        }
        if rotation.state == SessionState::None || rotation.refusals >= schedule.max_refusals {
            return None;
        }
        if let Some(at) = self.retry_at {
            return Some(at);
        }
        let scheduled = match self.scheduled {
            Some((rotated_at, due)) if rotated_at == rotation.rotated_at => due,
            _ => {
                let due = match rotation.rotated_at {
                    Some(at) => {
                        let due = at.saturating_add(schedule.interval.as_secs());
                        now + Duration::from_secs(due.saturating_sub(unix_now))
                    }
                    None => self.seen + FIRST_DELAY,
                };
                self.scheduled = Some((rotation.rotated_at, due));
                due
            }
        };
        if self.expired {
            let soonest = self
                .last_attempt
                .map_or(now, |at| (at + EXPIRY_SPACING).max(now));
            return Some(scheduled.min(soonest));
        }
        Some(scheduled)
    }

    fn attempted(&mut self, report: &RotationReport, schedule: RotationSchedule, now: Instant) {
        self.last_attempt = Some(now);
        self.expired = false;
        if matches!(report.outcome, RotationOutcome::Inconclusive(_)) {
            self.inconclusive += 1;
        } else {
            self.inconclusive = 0;
        }
        match &report.outcome {
            RotationOutcome::NoSession | RotationOutcome::Rotated { .. } => {
                self.retry_at = None;
                self.next_retry = FIRST_RETRY;
            }
            RotationOutcome::SignedOut(_) => {
                let n = usize::try_from(report.refusals.max(1) - 1).unwrap_or(usize::MAX);
                let wait = REFUSAL_WAITS[n.min(REFUSAL_WAITS.len() - 1)];
                self.retry_at = (!report.gave_up).then_some(now + wait);
            }
            RotationOutcome::Inconclusive(_) => {
                self.retry_at = Some(now + self.next_retry);
                self.next_retry = (self.next_retry * 2).min(schedule.interval.max(FIRST_RETRY));
            }
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// For tests: a visit's verdict without a browser.
#[cfg(test)]
fn visit_of(cookies: &CookieJar, logged_in: Option<bool>, host: &str, rotations: &[u16]) -> Visit {
    Visit {
        accepted: cookies.clone(),
        cookies: cookies.clone(),
        logged_in,
        host: host.to_owned(),
        rotations: rotations.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio::sync::mpsc;

    use super::*;
    use crate::youtube::cookies::tests::SIGNED_IN;

    fn signed_in() -> CookieJar {
        SIGNED_IN.parse().unwrap()
    }

    #[test]
    fn verdicts_rest_on_what_youtube_showed() {
        let jar = signed_in();
        let rotated = |visit: &Visit| judge(&jar, visit);
        assert_eq!(
            rotated(&visit_of(&jar, Some(true), "www.youtube.com", &[])),
            RotationOutcome::Rotated {
                tokens_changed: false
            }
        );
        assert!(matches!(
            rotated(&visit_of(&jar, Some(false), "www.youtube.com", &[])),
            RotationOutcome::SignedOut(_)
        ));
        assert!(matches!(
            rotated(&visit_of(&jar, None, "www.youtube.com", &[])),
            RotationOutcome::Inconclusive(_)
        ));
        assert!(matches!(
            rotated(&visit_of(&jar, None, "accounts.google.com", &[])),
            RotationOutcome::SignedOut(_)
        ));
        assert!(matches!(
            rotated(&visit_of(&jar, Some(true), "www.youtube.com", &[401])),
            RotationOutcome::SignedOut(_)
        ));
        assert!(
            matches!(
                rotated(&visit_of(&jar, Some(true), "www.youtube.com", &[401, 200])),
                RotationOutcome::Rotated { .. }
            ),
            "a retry that went through counts"
        );
        assert_eq!(
            rotated(&visit_of(&jar, Some(true), "www.youtube.com", &[429])),
            RotationOutcome::Rotated {
                tokens_changed: false
            },
            "Google declining to rotate fresh tokens again is no refusal"
        );

        let cleared: CookieJar = SIGNED_IN
            .lines()
            .filter(|l| !l.contains("LOGIN_INFO"))
            .map(|l| format!("{l}\n"))
            .collect::<String>()
            .parse()
            .unwrap();
        let mut signed_out = visit_of(&jar, Some(true), "www.youtube.com", &[]);
        signed_out.cookies = cleared.clone();
        assert!(matches!(
            rotated(&signed_out),
            RotationOutcome::SignedOut(_)
        ));
        assert!(
            matches!(
                rotated(&visit_of(&cleared, Some(false), "www.youtube.com", &[])),
                RotationOutcome::Inconclusive(_)
            ),
            "a browser that did not take the login cookies says nothing about the session"
        );

        let mut fresh = visit_of(&jar, Some(true), "www.youtube.com", &[200]);
        fresh.cookies = SIGNED_IN
            .replace("__Secure-3PSID\t", "__Secure-3PSIDTS\t")
            .parse()
            .unwrap();
        assert_eq!(
            rotated(&fresh),
            RotationOutcome::Rotated {
                tokens_changed: true
            }
        );
    }

    struct Harness {
        dir: tempfile::TempDir,
        store: Arc<SessionStore>,
        attempts: mpsc::UnboundedReceiver<Duration>,
        outcomes: Arc<Mutex<Vec<RotationOutcome>>>,
        start: Instant,
    }

    const SCHEDULE: RotationSchedule = RotationSchedule {
        interval: Duration::from_secs(6 * 3600),
        max_refusals: 3,
    };

    /// The scheduler with a fake browser that answers with `outcomes` in turn (rotating once
    /// they run out); each attempt reports when it happened.
    fn harness(outcomes: Vec<RotationOutcome>) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SessionStore::new(dir.path().join("youtube")));
        store.import(SIGNED_IN).unwrap();
        let (sent, attempts) = mpsc::unbounded_channel();
        let outcomes = Arc::new(Mutex::new(outcomes));
        let start = Instant::now();
        tokio::spawn({
            let store = Arc::clone(&store);
            let outcomes = Arc::clone(&outcomes);
            async move {
                run_schedule(&store.clone(), SCHEDULE, || {
                    let store = Arc::clone(&store);
                    let outcomes = Arc::clone(&outcomes);
                    let sent = sent.clone();
                    async move {
                        sent.send(start.elapsed()).unwrap();
                        let outcome = {
                            let mut outcomes = outcomes.lock().unwrap();
                            if outcomes.is_empty() {
                                RotationOutcome::Rotated {
                                    tokens_changed: true,
                                }
                            } else {
                                outcomes.remove(0)
                            }
                        };
                        let generation = store.rotation().unwrap().generation;
                        record(&store, generation, outcome, SCHEDULE.max_refusals)
                    }
                })
                .await;
            }
        });
        Harness {
            dir,
            store,
            attempts,
            outcomes,
            start,
        }
    }

    impl Harness {
        /// When the next attempt happens, from the start.
        async fn next(&mut self) -> Duration {
            self.attempts.recv().await.unwrap()
        }

        /// Whether an attempt happens within `within` from now.
        async fn attempt_within(&mut self, within: Duration) -> bool {
            tokio::time::timeout(within, self.attempts.recv())
                .await
                .is_ok()
        }
    }

    fn approx(actual: Duration, expected: Duration) {
        let slack = Duration::from_secs(5);
        assert!(
            actual + slack >= expected && actual <= expected + slack,
            "{actual:?} is not about {expected:?}"
        );
    }

    const MIN: Duration = Duration::from_secs(60);

    #[tokio::test(start_paused = true)]
    async fn rotates_soon_after_import_then_on_schedule() {
        let mut h = harness(vec![]);
        approx(h.next().await, FIRST_DELAY);
        approx(h.next().await, FIRST_DELAY + SCHEDULE.interval);
        assert!(h.store.status().unwrap().rotated_at.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_refusals_give_up_and_expire_the_session() {
        let refused = || RotationOutcome::SignedOut("signed out".into());
        let mut h = harness(vec![refused(), refused(), refused()]);
        approx(h.next().await, FIRST_DELAY);
        approx(h.next().await, FIRST_DELAY + 5 * MIN);
        approx(h.next().await, FIRST_DELAY + 35 * MIN);
        assert!(
            !h.attempt_within(Duration::from_secs(48 * 3600)).await,
            "gave up"
        );
        assert_eq!(h.store.status().unwrap().state, SessionState::Expired);

        // fresh cookies start over
        h.store.import(SIGNED_IN).unwrap();
        let imported = h.start.elapsed();
        approx(h.next().await, imported + FIRST_DELAY);
    }

    #[tokio::test(start_paused = true)]
    async fn failures_on_our_side_are_retried_and_never_held_against_the_session() {
        let flaky = || RotationOutcome::Inconclusive("no network".into());
        let mut h = harness((0..12).map(|_| flaky()).collect());
        let mut expected = FIRST_DELAY;
        let mut wait = FIRST_RETRY;
        approx(h.next().await, expected);
        for _ in 0..12 {
            expected += wait;
            wait = (wait * 2).min(SCHEDULE.interval);
            approx(h.next().await, expected);
        }
        assert_eq!(h.store.status().unwrap().state, SessionState::Unverified);
        assert_eq!(h.store.rotation().unwrap().refusals, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_success_forgives_earlier_refusals() {
        let refused = || RotationOutcome::SignedOut("signed out".into());
        let mut h = harness(vec![refused(), refused()]);
        h.next().await;
        h.next().await;
        // the third try rotates: the count starts over
        h.next().await;
        assert_eq!(h.store.rotation().unwrap().refusals, 0);
        h.outcomes.lock().unwrap().extend([refused(), refused()]);
        h.next().await;
        h.next().await;
        assert!(h.attempt_within(Duration::from_secs(3600)).await);
        assert_ne!(h.store.status().unwrap().state, SessionState::Expired);
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_session_is_rotated_early_but_not_over_and_over() {
        let mut h = harness(vec![]);
        h.next().await;
        let generation = h.store.rotation().unwrap().generation;

        tokio::time::sleep(Duration::from_secs(2 * 3600)).await;
        h.store
            .record(generation, SessionState::Expired, true)
            .unwrap();
        let expired = h.start.elapsed();
        approx(h.next().await, expired);

        // expired again right away: the next try waits a while
        h.store
            .record(generation, SessionState::Valid, true)
            .unwrap();
        h.store
            .record(generation, SessionState::Expired, true)
            .unwrap();
        approx(h.next().await, expired + EXPIRY_SPACING);
    }

    #[tokio::test(start_paused = true)]
    async fn imports_by_other_processes_are_noticed() {
        let mut h = harness(vec![]);
        h.next().await;
        // another process's store: its import reaches this one only through the files
        let other = SessionStore::new(h.dir.path().join("youtube"));
        tokio::time::sleep(Duration::from_secs(3600)).await;
        other.import(SIGNED_IN).unwrap();
        let imported = h.start.elapsed();
        let next = h.next().await;
        assert!(
            next > imported && next <= imported + POLL + FIRST_DELAY + Duration::from_secs(5),
            "{next:?} after an import at {imported:?}"
        );
    }

    #[test]
    fn runs_of_inconclusive_attempts_are_counted() {
        let now = Instant::now();
        let mut plan = Plan::new(Some(1), now);
        let flaky = RotationReport::new(RotationOutcome::Inconclusive("no flag".into()));
        for _ in 0..INCONCLUSIVE_WARNING {
            plan.attempted(&flaky, SCHEDULE, now);
        }
        assert_eq!(plan.inconclusive, INCONCLUSIVE_WARNING);
        let refused = RotationReport::new(RotationOutcome::SignedOut("signed out".into()));
        plan.attempted(&refused, SCHEDULE, now);
        assert_eq!(plan.inconclusive, 0, "YouTube answered");
    }

    #[test]
    fn plans_follow_the_stored_rotation_time() {
        let now = Instant::now();
        let state = RotationState {
            generation: 1,
            state: SessionState::Valid,
            rotated_at: Some(1_000_000),
            refusals: 0,
        };
        // rotated two hours ago by an earlier run of the engine
        let mut plan = Plan::new(None, now);
        let due = plan
            .due(&state, SCHEDULE, now, 1_000_000 + 2 * 3600)
            .unwrap();
        assert_eq!(due - now, Duration::from_secs(4 * 3600));
        let later = now + Duration::from_secs(600);
        assert_eq!(
            plan.due(&state, SCHEDULE, later, 1_000_000 + 2 * 3600)
                .unwrap(),
            due,
            "the deadline holds"
        );
        // overdue: right away
        let mut plan = Plan::new(None, now);
        let due = plan
            .due(&state, SCHEDULE, now, 1_000_000 + 7 * 3600)
            .unwrap();
        assert_eq!(due, now);
        let gave_up = RotationState {
            refusals: 3,
            ..state
        };
        assert_eq!(plan.due(&gave_up, SCHEDULE, now, 0), None);
    }

    /// The real browser against a page that plays YouTube: it sets a rotated token, reports
    /// itself signed in, and links media and fonts that must be blocked.
    #[tokio::test]
    #[ignore = "needs chromium"]
    async fn needs_chromium_rotates_through_a_real_browser() {
        use axum::Router;
        use axum::response::{Html, IntoResponse};
        use axum::routing::get;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let heavy = Arc::new(AtomicUsize::new(0));
        let counted = {
            let heavy = Arc::clone(&heavy);
            move || {
                heavy.fetch_add(1, Ordering::SeqCst);
                async { "" }
            }
        };
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    (
                        [("set-cookie", "SIDCC=rotated; Path=/; Max-Age=86400")],
                        Html(
                            "<link rel=stylesheet href=/style.css><img src=/a.png>\
                             <video src=/v.webm autoplay></video>\
                             <script>window.ytcfg = { get: k => k === 'LOGGED_IN' ? true : undefined };</script>",
                        ),
                    )
                        .into_response()
                }),
            )
            .route("/style.css", get(counted.clone()))
            .route("/a.png", get(counted.clone()))
            .route("/v.webm", get(counted));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let program = find_chromium(std::env::var("ENTROXIMER_CHROMIUM").ok().as_deref())
            .expect("a Chromium on PATH or ENTROXIMER_CHROMIUM");
        let config = BrowserConfig {
            program,
            no_sandbox: std::env::var_os("ENTROXIMER_CHROMIUM_NO_SANDBOX").is_some(),
            proxy: None,
        };
        // a session for the page's host, with a session cookie; expiring within the 400 days
        // Chromium allows
        let expires = unix_now() + 30 * 86_400;
        let jar: CookieJar = format!(
            "# Netscape HTTP Cookie File\n\
             #HttpOnly_127.0.0.1\tFALSE\t/\tFALSE\t{expires}\tLOGIN_INFO\tlogin\n\
             127.0.0.1\tFALSE\t/\tFALSE\t0\tPREF\tf6=1\n"
        )
        .parse()
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let start = Url::parse(&format!("http://{addr}/")).unwrap();
        let visit = visit(&config, dir.path(), &jar, &start).await.unwrap();
        assert_eq!(visit.logged_in, Some(true));
        assert_eq!(visit.host, "127.0.0.1");
        for cookie in jar.iter() {
            assert!(
                visit.accepted.iter().any(|c| c == cookie),
                "the browser took {cookie:?} as it was, not as {:?}",
                visit.accepted
            );
        }
        assert!(
            visit
                .cookies
                .iter()
                .any(|c| c.name == "SIDCC" && c.value == "rotated")
        );
        assert_eq!(
            heavy.load(Ordering::SeqCst),
            0,
            "heavy requests are blocked"
        );
        let profiles = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(profiles, 0, "the profile is removed");
    }
}
