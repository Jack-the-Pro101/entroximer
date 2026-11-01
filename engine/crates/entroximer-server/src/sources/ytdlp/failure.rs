//! What a failed yt-dlp run means, judged from its error output.

/// Why YouTube wants a signed-in account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restriction {
    AgeRestricted,
    /// "Sign in to confirm you're not a bot", or a captcha.
    BotCheck,
    MembersOnly,
    Private,
    /// Any other sign-in requirement.
    SignIn,
}

impl Restriction {
    pub fn describe(self) -> &'static str {
        match self {
            Self::AgeRestricted => "is age-restricted",
            Self::BotCheck => "is blocked by YouTube's bot check",
            Self::MembersOnly => "is for channel members only",
            Self::Private => "is private",
            Self::SignIn => "requires signing in to YouTube",
        }
    }

    /// Whether YouTube still asking for this with a session means the session itself is bad,
    /// rather than the account lacking access (membership, an invitation to a private video).
    pub fn implicates_session(self) -> bool {
        matches!(self, Self::AgeRestricted | Self::BotCheck | Self::SignIn)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Signing in may make it playable.
    Restricted(Restriction),
    /// YouTube throttles this session or address for a while.
    RateLimited,
    /// Removed, terminated, geo-blocked or never existed.
    Unavailable,
    Other,
}

/// yt-dlp's warning when YouTube ends a session it was given (cookies rotated or revoked).
pub const SESSION_ENDED_WARNING: &str = "account cookies are no longer valid";

/// Classifies yt-dlp's error output. Order matters: e.g. private videos also say "sign in".
pub fn classify(message: &str) -> Failure {
    let message = message.to_lowercase();
    let mentions = |needles: &[&str]| needles.iter().any(|n| message.contains(n));
    if mentions(&["rate-limited", "try again later"]) {
        Failure::RateLimited
    } else if mentions(&["private video", "video is private"]) {
        Failure::Restricted(Restriction::Private)
    } else if mentions(&[
        "members-only",
        "members only",
        "channel's members",
        "join this channel",
    ]) {
        Failure::Restricted(Restriction::MembersOnly)
    } else if mentions(&[
        "confirm your age",
        "age-restricted",
        "inappropriate for some users",
    ]) {
        Failure::Restricted(Restriction::AgeRestricted)
    } else if mentions(&["not a bot", "captcha"]) {
        Failure::Restricted(Restriction::BotCheck)
    } else if mentions(&[
        "sign in",
        "registered users",
        "login required",
        "requires authentication",
    ]) {
        Failure::Restricted(Restriction::SignIn)
    } else if mentions(&[
        "unavailable",
        "removed",
        "terminated",
        "does not exist",
        "not available",
    ]) {
        Failure::Unavailable
    } else {
        Failure::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_yt_dlp_messages() {
        let cases = [
            // captured from yt-dlp 2026.10.26
            (
                "ERROR: [youtube] Tq92D6wQ1mg: Sign in to confirm your age. This video may be inappropriate for some users. Use --cookies-from-browser or --cookies for the authentication.",
                Failure::Restricted(Restriction::AgeRestricted),
            ),
            (
                "ERROR: [youtube] Cr381pDsSsA: This video has been removed for violating YouTube's Terms of Service",
                Failure::Unavailable,
            ),
            (
                "ERROR: [youtube] x: Sign in to confirm you\u{2019}re not a bot. Use --cookies-from-browser or --cookies for the authentication.",
                Failure::Restricted(Restriction::BotCheck),
            ),
            (
                "ERROR: [youtube] x: Private video. Sign in if you've been granted access to this video",
                Failure::Restricted(Restriction::Private),
            ),
            (
                "ERROR: [youtube] x: Join this channel to get access to members-only content like this video",
                Failure::Restricted(Restriction::MembersOnly),
            ),
            (
                "ERROR: [youtube] x: This content isn't available, try again later. The current session has been rate-limited by YouTube for up to an hour.",
                Failure::RateLimited,
            ),
            (
                "ERROR: [youtube] x: Video unavailable",
                Failure::Unavailable,
            ),
            (
                "ERROR: [youtube] x: This video is only available for registered users",
                Failure::Restricted(Restriction::SignIn),
            ),
            (
                "ERROR: Unable to download webpage: HTTP Error 500",
                Failure::Other,
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(classify(message), expected, "{message}");
        }
    }
}
