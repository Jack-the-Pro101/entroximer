//! YouTube withholds a video's media until its pre-roll ad could have been skipped: googlevideo
//! refuses requests for the media URL until then (2 to 3 s in November 2026 tests, none for
//! videos without an ad). yt-dlp reports the moment as `available_at`, in whole seconds, and
//! sleeps until it before downloading.
//!
//! The engine instead polls the URL with one-byte requests, starting shortly before
//! `available_at` (which YouTube's actual opening preceded by 1.9 s in every test), and starts
//! the download as soon as YouTube accepts it. The info JSON handed to
//! yt-dlp then leaves `available_at` out, so that yt-dlp does not sleep again; together that took
//! the time to first audio from 5 s down to the length of the ad.

use std::time::{Duration, Instant, SystemTime};

use reqwest::StatusCode;
use reqwest::header::RANGE;
use tokio::sync::OnceCell;

const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// URLs opened 1.9 s before `available_at` in tests (it is rounded up to whole seconds, and the
/// ad's skip time counts from an earlier moment); polling starts this much before it.
const START_EARLY: Duration = Duration::from_millis(2500);
/// Past this, the download starts anyway and reports whatever the URL answers.
const GIVE_UP_AFTER: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(super) struct UrlActivation {
    url: String,
    headers: Vec<(String, String)>,
    /// When YouTube expects the URL to work (yt-dlp's `available_at`).
    expected: Option<SystemTime>,
    /// Set once the URL answered, so later opens of the track do not probe again.
    done: OnceCell<()>,
}

impl UrlActivation {
    pub(super) fn new(
        url: String,
        headers: Vec<(String, String)>,
        expected: Option<SystemTime>,
    ) -> Self {
        Self {
            url,
            headers,
            expected,
            done: OnceCell::new(),
        }
    }

    /// Returns once the URL accepts requests, or once waiting longer is pointless.
    pub(super) async fn wait(&self) {
        self.done.get_or_init(|| self.probe()).await;
    }

    async fn probe(&self) {
        if let Some(early) = self.expected.and_then(|expected| {
            expected
                .duration_since(SystemTime::now() + START_EARLY)
                .ok()
        }) {
            tokio::time::sleep(early).await;
        }
        let started = Instant::now();
        loop {
            match self.request().await {
                Ok(status) if status.is_success() => {
                    tracing::debug!(waited = ?started.elapsed(), "media URL accepted");
                    return;
                }
                Ok(StatusCode::FORBIDDEN) if started.elapsed() < GIVE_UP_AFTER => {
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
                // anything else is for the download itself to report
                Ok(status) => {
                    tracing::debug!(%status, "stopped probing the media URL");
                    return;
                }
                Err(e) => {
                    tracing::debug!("cannot probe the media URL: {e}");
                    return;
                }
            }
        }
    }

    async fn request(&self) -> reqwest::Result<StatusCode> {
        let mut request = crate::sources::net::client()
            .get(&self.url)
            .header(RANGE, "bytes=0-0")
            .timeout(REQUEST_TIMEOUT);
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        Ok(request.send().await?.status())
    }
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
        let activation = UrlActivation::new(url, vec![("User-Agent".into(), "test".into())], None);

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
        let activation = UrlActivation::new("http://127.0.0.1:9/media".into(), Vec::new(), None);
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
        let activation = UrlActivation::new(url, Vec::new(), Some(expected));

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
}
