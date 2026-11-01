//! Signing in to YouTube only when a video needs it, against a fake yt-dlp that behaves like
//! YouTube's age gate: it fails without cookies, and rotates a cookie when given them.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use clap::Parser;
use entroximer_server::config::{MediaConfig, MediaContext};
use entroximer_server::sources::ytdlp::Restriction;
use entroximer_server::sources::{self, ResolveError};
use entroximer_server::youtube::{SessionState, SignInUnavailable};

const VIDEO: &str = "https://www.youtube.com/watch?v=restricted";

/// A minimal signed-in export with fake values.
const SIGNED_IN: &str = "# Netscape HTTP Cookie File
#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1830000000\tLOGIN_INFO\tlogin
.youtube.com\tTRUE\t/\tTRUE\t1830000000\t__Secure-3PAPISID\tsapisid
#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1830000000\t__Secure-3PSID\tpsid-original
";

const INFO_JSON: &str = r#"{"id":"restricted","title":"Restricted","extractor_key":"Youtube","duration":60,"formats":[{"format_id":"251","ext":"webm","acodec":"opus","vcodec":"none","abr":128,"protocol":"https","url":"https://example.invalid/a"}]}"#;

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    media: MediaConfig,
}

struct Engine {
    dir: tempfile::TempDir,
    ctx: MediaContext,
}

impl Engine {
    /// `with_cookies` is the shell run by the fake yt-dlp when it was given `--cookies`.
    fn new(extra_args: &[&str], with_cookies: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("yt-dlp");
        let calls = dir.path().join("calls");
        fs::write(
            &script,
            format!(
                r#"#!/bin/sh
cookies=""
previous=""
for arg in "$@"; do
    [ "$previous" = "--cookies" ] && cookies="$arg"
    previous="$arg"
done
if [ -n "$cookies" ]; then echo signed-in; else echo anonymous; fi >> '{calls}'
if [ -z "$cookies" ]; then
    echo "ERROR: [youtube] restricted: Sign in to confirm your age. This video may be inappropriate for some users." >&2
    exit 1
fi
{with_cookies}
"#,
                calls = calls.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        let path = |name: &str| dir.path().join(name).to_string_lossy().into_owned();
        let mut args = vec![
            "entroximer".to_owned(),
            "--ytdlp".into(),
            script.to_string_lossy().into_owned(),
            "--data-dir".into(),
            path("data"),
            "--cache-dir".into(),
            path("cache"),
        ];
        args.extend(extra_args.iter().map(|a| (*a).to_owned()));
        let ctx = MediaContext::new(Cli::parse_from(args).media).unwrap();
        Self { dir, ctx }
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn stored_cookies(&self) -> String {
        fs::read_to_string(self.cookie_file()).unwrap()
    }

    fn cookie_file(&self) -> PathBuf {
        Path::new(&self.dir.path()).join("data/youtube/cookies.txt")
    }
}

/// Succeeds with cookies, rotating one of them like YouTube does.
fn succeed_and_rotate() -> String {
    format!("sed -i 's/psid-original/psid-rotated/' \"$cookies\"\nprintf '%s' '{INFO_JSON}'")
}

#[tokio::test]
async fn signs_in_only_for_videos_that_need_it() {
    let engine = Engine::new(&[], &succeed_and_rotate());

    // no session yet: one anonymous attempt, then a clear error
    let err = sources::resolve(VIDEO, &engine.ctx).await.unwrap_err();
    assert!(matches!(
        err,
        ResolveError::SignInRequired {
            restriction: Restriction::AgeRestricted,
            reason: SignInUnavailable::NoSession,
            ..
        }
    ));
    assert!(
        err.to_string()
            .contains("entroximer youtube cookies import")
    );
    assert_eq!(engine.calls(), ["anonymous"]);

    // the failed attempt showed the video needs an account, so the next one signs in directly
    engine.ctx.youtube.store.import(SIGNED_IN).unwrap();
    let track = sources::resolve(VIDEO, &engine.ctx).await.unwrap();
    assert_eq!(track.info.title, "Restricted");
    assert_eq!(engine.calls(), ["anonymous", "signed-in"]);

    // the cookie YouTube rotated reaches the stored session once the track is done
    drop(track);
    assert!(engine.stored_cookies().contains("psid-rotated"));
    let mode = fs::metadata(engine.cookie_file())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    let _again = sources::resolve(VIDEO, &engine.ctx).await.unwrap();
    assert_eq!(engine.calls(), ["anonymous", "signed-in", "signed-in"]);
}

#[tokio::test]
async fn unrestricted_videos_never_use_the_session() {
    let engine = Engine::new(&[], &succeed_and_rotate());
    engine.ctx.youtube.store.import(SIGNED_IN).unwrap();

    let track = sources::resolve(VIDEO, &engine.ctx).await.unwrap();
    assert_eq!(engine.calls(), ["anonymous", "signed-in"], "fell back once");
    drop(track);

    // a different video: tried anonymously first again
    let other = "https://www.youtube.com/watch?v=other";
    let _ = sources::resolve(other, &engine.ctx).await;
    assert_eq!(engine.calls()[2], "anonymous");
}

#[tokio::test]
async fn a_session_youtube_ended_expires() {
    let ended = "echo 'WARNING: [youtube] The provided YouTube account cookies are no longer valid. They have likely been rotated in the browser as a security measure.' >&2
echo 'ERROR: [youtube] restricted: Sign in to confirm your age. This video may be inappropriate for some users.' >&2
exit 1";
    let engine = Engine::new(&[], ended);
    engine.ctx.youtube.store.import(SIGNED_IN).unwrap();

    let err = sources::resolve(VIDEO, &engine.ctx).await.unwrap_err();
    assert!(matches!(
        err,
        ResolveError::SignInRequired {
            reason: SignInUnavailable::Refused,
            ..
        }
    ));
    let status = engine.ctx.youtube.store.status().unwrap();
    assert_eq!(status.state, SessionState::Expired);

    // an expired session is not sent again; the error says what to do
    let err = sources::resolve(VIDEO, &engine.ctx).await.unwrap_err();
    assert!(matches!(
        err,
        ResolveError::SignInRequired {
            reason: SignInUnavailable::Expired,
            ..
        }
    ));
    assert_eq!(engine.calls(), ["anonymous", "signed-in", "anonymous"]);
}

#[tokio::test]
async fn never_policy_keeps_the_account_out() {
    let engine = Engine::new(&["--youtube-auth", "never"], &succeed_and_rotate());
    engine.ctx.youtube.store.import(SIGNED_IN).unwrap();

    let err = sources::resolve(VIDEO, &engine.ctx).await.unwrap_err();
    assert!(matches!(
        err,
        ResolveError::SignInRequired {
            reason: SignInUnavailable::Disabled,
            ..
        }
    ));
    assert_eq!(engine.calls(), ["anonymous"]);
}
