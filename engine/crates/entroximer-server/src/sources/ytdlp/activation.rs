//! YouTube withholds a video's media until its pre-roll ad could have been skipped: googlevideo
//! refuses requests for the media URL until then. yt-dlp reports the moment as `available_at`, in
//! whole seconds (the ad's skip time from now, or its length if it cannot be skipped), and sleeps
//! until it before downloading. Most ads can be skipped after 5 s; unskippable ones hold the media
//! longer (16 s in November 2026 tests), and videos without an ad not at all.
//!
//! The engine instead polls the URL with one-byte requests, starting shortly before
//! `available_at`, and starts the download as soon as YouTube accepts it. The info JSON handed to
//! yt-dlp then leaves `available_at` out, so that yt-dlp does not sleep again; together that took
//! the time to first audio from 5 s down to the length of the ad. YouTube opened the URL earlier
//! the longer its wait: 1.9 s before `available_at` when 4 to 5 s of it were left once the track
//! was resolved, 4.9 s before when 15.5 s were. So polling starts once 60% of what was left has
//! passed, or 2.5 s before `available_at` if that is sooner.
//!
//! Meanwhile the track's producer reports the wait (`Producer::available_at`), so that waiting
//! out an ad does not count as a stall.

use std::time::{Duration, Instant, SystemTime};

use reqwest::StatusCode;
use reqwest::header::RANGE;
use tokio::sync::OnceCell;

const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Short waits ended 1.9 s before `available_at` in tests (it is rounded up to whole seconds,
/// and the ad's skip time counts from an earlier moment); polling starts at least this much
/// before it.
const START_EARLY: Duration = Duration::from_millis(2500);
/// Long waits end earlier still: polling starts once this share of the wait has passed.
const START_AFTER_SHARE: f64 = 0.6;
/// This long after polling started, and after `available_at`, the download starts anyway and
/// reports whatever the URL answers.
const GIVE_UP_AFTER: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(super) struct UrlActivation {
    /// Through the proxy yt-dlp got the URL with, since it only works from that address.
    client: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    /// When the track was resolved, about when YouTube's wait began.
    resolved: Instant,
    /// When YouTube expects the URL to work (yt-dlp's `available_at`).
    expected: Option<Instant>,
    /// When the URL answered, so that later opens of the track do not probe again.
    done: OnceCell<Instant>,
}

impl UrlActivation {
    pub(super) fn new(
        client: reqwest::Client,
        url: String,
        headers: Vec<(String, String)>,
        expected: Option<SystemTime>,
    ) -> Self {
        let resolved = Instant::now();
        let expected = expected.map(|expected| {
            resolved
                + expected
                    .duration_since(SystemTime::now())
                    .unwrap_or_default()
        });
        Self {
            client,
            url,
            headers,
            resolved,
            expected,
            done: OnceCell::new(),
        }
    }

    /// Returns once the URL accepts requests, or once waiting longer is pointless.
    pub(super) async fn wait(&self) {
        self.done.get_or_init(|| self.probe()).await;
    }

    /// When the URL started to work, or while it does not yet, when YouTube said it would.
    pub(super) fn available_at(&self) -> Option<Instant> {
        let expected = self.expected?;
        Some(self.done.get().copied().unwrap_or(expected))
    }

    /// Returns when the URL answered.
    async fn probe(&self) -> Instant {
        if let Some(expected) = self.expected {
            tokio::time::sleep_until(polling_start(self.resolved, expected).into()).await;
        }
        let started = Instant::now();
        let give_up = started.max(self.expected.unwrap_or(started)) + GIVE_UP_AFTER;
        loop {
            match self.request().await {
                Ok(status) if status.is_success() => {
                    tracing::debug!(waited = ?started.elapsed(), "media URL accepted");
                    return Instant::now();
                }
                Ok(StatusCode::FORBIDDEN) if Instant::now() < give_up => {
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
                // anything else is for the download itself to report
                Ok(status) => {
                    tracing::debug!(%status, "stopped probing the media URL");
                    return Instant::now();
                }
                Err(e) => {
                    tracing::debug!("cannot probe the media URL: {e}");
                    return Instant::now();
                }
            }
        }
    }

    async fn request(&self) -> reqwest::Result<StatusCode> {
        let mut request = self
            .client
            .get(&self.url)
            .header(RANGE, "bytes=0-0")
            .timeout(REQUEST_TIMEOUT);
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        Ok(request.send().await?.status())
    }
}

/// When to start polling for a URL resolved at `resolved` that YouTube expects to work at
/// `expected`; see the module.
fn polling_start(resolved: Instant, expected: Instant) -> Instant {
    let share = resolved
        + expected
            .saturating_duration_since(resolved)
            .mul_f64(START_AFTER_SHARE);
    let early = expected.checked_sub(START_EARLY).unwrap_or(resolved);
    share.min(early).max(resolved)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use axum::Router;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::get;

    use super::*;

    /// A server refusing the first `refusals` requests with 403, then answering 206.
    async fn server(refusals: u32) -> (String, Arc<AtomicU32>) {
        let requests = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route(
                "/media",
                get(move |State(requests): State<Arc<AtomicU32>>| async move {
                    if requests.fetch_add(1, Ordering::SeqCst) < refusals {
                        StatusCode::FORBIDDEN
                    } else {
                        StatusCode::PARTIAL_CONTENT
                    }
                }),
            )
            .with_state(Arc::clone(&requests));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/media", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        (url, requests)
    }

    #[tokio::test]
    async fn waits_until_the_url_is_accepted_and_only_once() {
        let (url, requests) = server(3).await;
        let activation = UrlActivation::new(
            crate::sources::net::client().clone(),
            url,
            vec![("User-Agent".into(), "test".into())],
            None,
        );

        let started = Instant::now();
        activation.wait().await;
        let waited = started.elapsed();
        assert_eq!(requests.load(Ordering::SeqCst), 4);
        assert!(
            waited >= 3 * POLL_INTERVAL,
            "returned early, after {waited:?}"
        );

        activation.wait().await;
        assert_eq!(
            requests.load(Ordering::SeqCst),
            4,
            "an accepted URL is not probed again"
        );
    }

    #[tokio::test]
    async fn other_answers_end_the_wait_at_once() {
        let activation = UrlActivation::new(
            crate::sources::net::client().clone(),
            "http://127.0.0.1:9/media".into(),
            Vec::new(),
            None,
        );
        let started = Instant::now();
        activation.wait().await;
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a refused connection is not retried"
        );
    }

    #[tokio::test]
    async fn polling_starts_shortly_before_the_expected_time() {
        let (url, requests) = server(0).await;
        let expected = SystemTime::now() + START_EARLY + Duration::from_millis(500);
        let activation = UrlActivation::new(
            crate::sources::net::client().clone(),
            url,
            Vec::new(),
            Some(expected),
        );

        let started = Instant::now();
        activation.wait().await;
        assert!(
            started.elapsed() >= Duration::from_millis(400),
            "polled too early"
        );
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "no requests before the expected time"
        );
    }

    #[test]
    fn polling_starts_earlier_for_longer_waits() {
        let resolved = Instant::now();
        let secs = Duration::from_secs_f64;
        // a 5 s ad, 4.3 s of it left: YouTube opened the URL 1.85 s before `available_at`
        let expected = resolved + secs(4.3);
        assert_eq!(polling_start(resolved, expected), expected - START_EARLY);
        // a 16 s one, 15.5 s left: opened 4.9 s before it, after 10.6 s
        let start = polling_start(resolved, resolved + secs(15.5));
        assert_eq!(start, resolved + secs(15.5 * START_AFTER_SHARE));
        assert!(start < resolved + secs(10.6));
        // already due
        assert_eq!(polling_start(resolved, resolved), resolved);
    }

    #[tokio::test]
    async fn tells_when_the_url_works() {
        let (url, _) = server(2).await;
        let client = crate::sources::net::client().clone();
        let unannounced = UrlActivation::new(client.clone(), url.clone(), Vec::new(), None);
        assert_eq!(unannounced.available_at(), None, "no wait was announced");

        let expected = SystemTime::now() + Duration::from_secs(1);
        let activation = UrlActivation::new(client, url, Vec::new(), Some(expected));
        let announced = activation.available_at().expect("a wait was announced");
        assert!(announced > Instant::now() + Duration::from_millis(900));

        // the server accepts the third request, sooner than announced
        activation.wait().await;
        let opened = activation.available_at().unwrap();
        assert!(opened < announced && opened <= Instant::now(), "{opened:?}");
    }
}
