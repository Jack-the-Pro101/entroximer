//! Signing the engine into YouTube with an exported cookie session, used only when needed.
//!
//! YouTube no longer accepts OAuth from yt-dlp, so the session is a set of browser cookies,
//! exported from a private window and imported with `entroximer youtube cookies import`. Using
//! an account risks YouTube restricting it, so by default ([`AuthPolicy::Fallback`]) every video
//! is tried anonymously first and the session is only sent when YouTube demands it.

pub mod command;
pub mod cookies;
pub mod session;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::ValueEnum;
use entroximer_core::error::display_chain;
use reqwest::Url;
use tokio::sync::{Notify, broadcast};

pub use session::{
    ImportReport, Session, SessionCopy, SessionError, SessionState, SessionStatus, SessionStore,
};

use crate::config::MediaContext;
use crate::expiring::Expiring;
use crate::sources::ytdlp::{self, Failure, Restriction, SESSION_ENDED_WARNING};
use crate::sources::{ResolveError, net};

/// When the YouTube session is sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum AuthPolicy {
    /// Never; videos that need an account fail.
    Never,
    /// Only for videos that need it, keeping the account out of most traffic.
    #[default]
    Fallback,
    /// For every YouTube request.
    Always,
}

/// Why a video that needs an account could not use one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignInUnavailable {
    Disabled,
    NoSession,
    Expired,
    /// YouTube asked for an account even with the session.
    Refused,
}

impl SignInUnavailable {
    pub fn advice(self) -> &'static str {
        match self {
            Self::Disabled => "signing in to YouTube is disabled (ENTROXIMER_YOUTUBE_AUTH=never)",
            Self::NoSession => {
                "sign the engine into YouTube to play it: `entroximer youtube cookies import`"
            }
            Self::Expired => {
                "the YouTube session has expired; import fresh cookies with `entroximer youtube cookies import`"
            }
            Self::Refused => "YouTube refused the signed-in account as well",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    NoSession,
    Valid,
    Expired,
    /// The check itself failed (network, rate limit, check video gone); the state is unchanged.
    Inconclusive(String),
}

#[derive(Debug)]
pub struct YouTube {
    pub store: Arc<SessionStore>,
    pub policy: AuthPolicy,
    /// An age-restricted video that only plays when signed in.
    check_video: String,
    /// Queries that needed an account recently, so repeat plays skip the anonymous attempt.
    recently_restricted: Expiring<String, ()>,
    check_requested: Notify,
    changes: broadcast::Sender<SessionState>,
    /// What everything sent to YouTube goes through, if set.
    proxy: Option<Url>,
    /// For the engine's own requests to YouTube: through `proxy`, if set.
    client: reqwest::Client,
}

impl YouTube {
    /// After YouTube refuses the session, at most this often is it checked on demand.
    const ON_DEMAND_CHECK_INTERVAL: Duration = Duration::from_secs(10 * 60);
    const REMEMBER_RESTRICTED_FOR: Duration = Duration::from_secs(24 * 3600);
    const REMEMBERED_RESTRICTED: usize = 4096;

    pub fn new(
        dir: PathBuf,
        policy: AuthPolicy,
        check_video: String,
        proxy: Option<Url>,
    ) -> reqwest::Result<Self> {
        let client = match &proxy {
            Some(proxy) => net::proxied_client(proxy)?,
            None => net::client().clone(),
        };
        Ok(Self {
            store: Arc::new(SessionStore::new(dir)),
            policy,
            check_video,
            recently_restricted: Expiring::new(
                Self::REMEMBER_RESTRICTED_FOR,
                Self::REMEMBERED_RESTRICTED,
            ),
            check_requested: Notify::new(),
            changes: broadcast::Sender::new(16),
            proxy,
            client,
        })
    }

    /// The proxy for everything sent to YouTube, if one is set.
    pub fn proxy(&self) -> Option<&Url> {
        self.proxy.as_ref()
    }

    /// The HTTP client for the engine's own requests to YouTube.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Changes of the session's state observed by this process.
    pub fn subscribe(&self) -> broadcast::Receiver<SessionState> {
        self.changes.subscribe()
    }

    /// Whether the first attempt at `query` should already be signed in.
    pub fn sign_in_first(&self, query: &str) -> bool {
        match self.policy {
            AuthPolicy::Never => false,
            AuthPolicy::Fallback => self.recently_restricted.get(query).is_some(),
            AuthPolicy::Always => true,
        }
    }

    pub fn remember_restricted(&self, query: &str) {
        self.recently_restricted.insert(query.to_owned(), ());
    }

    /// A session copy for one yt-dlp run, if the policy allows one and a usable session exists.
    pub fn checkout(&self, ctx: &MediaContext) -> Result<Option<SessionCopy>, SessionError> {
        if self.policy == AuthPolicy::Never {
            return Ok(None);
        }
        self.store.checkout(ctx.work_dir())
    }

    /// Why no session could be used.
    pub fn unavailable_reason(&self) -> SignInUnavailable {
        match self.policy {
            AuthPolicy::Never => SignInUnavailable::Disabled,
            _ => match self.store.status().map(|s| s.state) {
                Ok(SessionState::Expired) => SignInUnavailable::Expired,
                _ => SignInUnavailable::NoSession,
            },
        }
    }

    /// Records what a yt-dlp run with `copy` showed about the session.
    pub fn observe(&self, copy: &SessionCopy, stderr: &str) {
        if stderr.contains(SESSION_ENDED_WARNING) {
            self.set_state(copy.generation(), SessionState::Expired, false);
        }
    }

    /// YouTube asked for an account even with the session: have the session checked soon.
    pub fn request_check(&self) {
        self.check_requested.notify_one();
    }

    fn set_state(&self, generation: u64, state: SessionState, checked: bool) {
        match self.store.record(generation, state, checked) {
            Ok(Some(previous)) => {
                // nobody listening (e.g. in the CLI) is fine
                let _ = self.changes.send(state);
                if state == SessionState::Expired {
                    tracing::warn!(
                        ?previous,
                        "the YouTube session has expired; import fresh cookies with `entroximer youtube cookies import`"
                    );
                } else {
                    tracing::info!(?previous, ?state, "YouTube session state changed");
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %display_chain(&e), "cannot record the YouTube session state")
            }
        }
    }
}

/// Checks the session by loading an age-restricted video with it. `even_if_expired` re-checks
/// a session already considered expired.
pub async fn check_session(
    ctx: &MediaContext,
    even_if_expired: bool,
) -> Result<CheckOutcome, SessionError> {
    let youtube = &ctx.youtube;
    let copy = if even_if_expired {
        youtube.store.checkout_any(ctx.work_dir())?
    } else {
        youtube.store.checkout(ctx.work_dir())?
    };
    let Some(copy) = copy else {
        return Ok(CheckOutcome::NoSession);
    };
    let outcome = match ytdlp::probe(ctx, &youtube.check_video, &copy).await {
        Ok(stderr) if stderr.contains(SESSION_ENDED_WARNING) => CheckOutcome::Expired,
        Ok(_) => CheckOutcome::Valid,
        // a bot check targets the server's address, which a working session does not always
        // get past; a session YouTube ended is reported by yt-dlp's warning instead
        Err(ResolveError::Extraction {
            failure: Failure::Restricted(restriction),
            ..
        }) if restriction.implicates_session() && restriction != Restriction::BotCheck => {
            CheckOutcome::Expired
        }
        Err(e) => CheckOutcome::Inconclusive(display_chain(&e)),
    };
    match outcome {
        CheckOutcome::Valid => youtube.set_state(copy.generation(), SessionState::Valid, true),
        CheckOutcome::Expired => youtube.set_state(copy.generation(), SessionState::Expired, true),
        CheckOutcome::NoSession | CheckOutcome::Inconclusive(_) => {}
    }
    // dropping the copy merges cookies YouTube rotated during the check, keeping them fresh
    drop(copy);
    Ok(outcome)
}

/// Checks the session now, every `interval`, and soon after YouTube refuses it (but not more
/// often than every few minutes). A session considered expired is checked too, so that one
/// misleading check does not sign the engine out for good. Runs forever.
pub async fn maintain_session(ctx: Arc<MediaContext>, interval: Duration) {
    loop {
        match check_session(&ctx, true).await {
            Ok(CheckOutcome::NoSession) => {}
            Ok(CheckOutcome::Valid) => tracing::debug!("the YouTube session works"),
            Ok(CheckOutcome::Expired) => {}
            Ok(CheckOutcome::Inconclusive(reason)) => {
                tracing::warn!(%reason, "could not check the YouTube session");
            }
            Err(e) => {
                tracing::warn!(error = %display_chain(&e), "could not check the YouTube session")
            }
        }
        let checked_at = tokio::time::Instant::now();
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            () = ctx.youtube.check_requested.notified() => {
                let spacing = YouTube::ON_DEMAND_CHECK_INTERVAL.min(interval);
                tokio::time::sleep_until(checked_at + spacing).await;
            }
        }
    }
}
