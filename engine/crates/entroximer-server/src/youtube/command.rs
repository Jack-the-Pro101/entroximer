//! `entroximer youtube cookies ...`: managing the YouTube session from the command line, which
//! also works against a running engine in a container:
//!
//! ```sh
//! ssh host 'podman exec -i entroximer entroximer youtube cookies import -' < cookies.txt
//! ```

use std::io::Read;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use clap::Subcommand;

use super::{CheckOutcome, SessionState, check_session};
use crate::config::MediaContext;

#[derive(Debug, Subcommand)]
pub enum YouTubeCommand {
    /// Manage the YouTube session (cookies of a signed-in browser session).
    #[command(subcommand)]
    Cookies(CookiesCommand),
}

#[derive(Debug, Subcommand)]
pub enum CookiesCommand {
    /// Replace the session with a cookies.txt export and check that it works.
    ///
    /// Export from a private browser window signed into a dedicated YouTube account, then
    /// close that window without signing out, so YouTube does not rotate the cookies.
    Import {
        /// Netscape cookies.txt file, or `-` for stdin.
        file: PathBuf,
        /// Skip checking the session with YouTube after importing.
        #[arg(long)]
        no_check: bool,
    },
    /// Show whether the session works and when it was last checked.
    Status,
    /// Check now whether YouTube still accepts the session.
    Check,
    /// Forget the session.
    Remove,
}

pub async fn run(command: YouTubeCommand, ctx: &MediaContext) -> anyhow::Result<()> {
    let YouTubeCommand::Cookies(command) = command;
    let store = &ctx.youtube.store;
    match command {
        CookiesCommand::Import { file, no_check } => {
            let text = read_input(&file)?;
            let report = store.import(&text).context("cannot import the cookies")?;
            println!(
                "imported {} cookies{}",
                report.cookies,
                match report.dropped {
                    0 => String::new(),
                    n => format!(" ({n} from other sites left out)"),
                }
            );
            if !no_check {
                print_check(check_session(ctx, true).await?);
            }
        }
        CookiesCommand::Status => {
            let status = store.status()?;
            let now = unix_now();
            println!("state: {}", describe(status.state));
            if let Some(at) = status.imported_at {
                println!("imported: {}", relative(at, now));
            }
            match status.checked_at {
                Some(at) => println!("last checked: {}", relative(at, now)),
                None if status.state != SessionState::None => println!("last checked: never"),
                None => {}
            }
            if let Some(at) = status.expires_at {
                println!("cookies expire: {}", relative(at, now));
            }
        }
        CookiesCommand::Check => print_check(check_session(ctx, true).await?),
        CookiesCommand::Remove => {
            if store.remove()? {
                println!("signed out of YouTube");
            } else {
                println!("there was no YouTube session");
            }
        }
    }
    Ok(())
}

fn read_input(file: &PathBuf) -> anyhow::Result<String> {
    if file.as_os_str() == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .context("cannot read cookies from stdin")?;
        Ok(text)
    } else {
        std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))
    }
}

fn print_check(outcome: CheckOutcome) {
    match outcome {
        CheckOutcome::NoSession => println!("there is no YouTube session to check"),
        CheckOutcome::Valid => println!("YouTube accepts the session"),
        CheckOutcome::Expired => println!(
            "YouTube does not accept the session: it has expired or the account cannot watch \
             age-restricted videos; import fresh cookies of an adult account"
        ),
        CheckOutcome::Inconclusive(reason) => {
            println!("could not check the session, it is unchanged: {reason}");
        }
    }
}

fn describe(state: SessionState) -> &'static str {
    match state {
        SessionState::None => "not signed in",
        SessionState::Unverified => "imported, not checked yet",
        SessionState::Valid => "signed in",
        SessionState::Expired => "expired, import fresh cookies",
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `3 hours ago`, `in 12 days`.
fn relative(at: u64, now: u64) -> String {
    let (seconds, past) = if at <= now {
        (now - at, true)
    } else {
        (at - now, false)
    };
    let (amount, unit) = match seconds {
        s if s < 120 => {
            return if past {
                "just now".into()
            } else {
                "in a moment".into()
            };
        }
        s if s < 2 * 3600 => (s / 60, "minutes"),
        s if s < 2 * 86_400 => (s / 3600, "hours"),
        s => (s / 86_400, "days"),
    };
    if past {
        format!("{amount} {unit} ago")
    } else {
        format!("in {amount} {unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times_read_naturally() {
        let now = 1_000_000_000;
        assert_eq!(relative(now - 30, now), "just now");
        assert_eq!(relative(now - 3 * 3600, now), "3 hours ago");
        assert_eq!(relative(now + 12 * 86_400, now), "in 12 days");
        assert_eq!(relative(now - 90 * 60, now), "90 minutes ago");
    }
}
