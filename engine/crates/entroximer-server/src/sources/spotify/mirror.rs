//! Finding a Spotify track's recording on YouTube.
//!
//! Spotify only describes music (its audio is DRM-protected), so its tracks play from YouTube.
//! The operator's mirror searches (by default the track's ISRC, then YouTube Music, then
//! YouTube) are tried in turn, and the best of their results that matches is played. A result
//! matches when
//! - its title shares at least half of the track's words (searches by ISRC find unrelated
//!   videos too, so even their results must),
//! - it is the same kind of recording: a "live", "remix", "cover", ... on one side only rules it
//!   out,
//! - its length, when both are known, is within 7 s or 10 % of the track's.

use crate::api::protocol::Millis;
use crate::sources::TrackInfo;

/// Results considered per mirror search.
pub(super) const CANDIDATES: usize = 5;

/// The default mirror searches, in order.
pub(crate) const DEFAULT_SEARCHES: &[&str] = &[
    "ytsearch:\"{isrc}\"",
    "ytmsearch:{artist} {title}",
    "ytsearch:{artist} - {title}",
];

/// Words of titles that say nothing about which recording it is.
const NOISE: &[&str] = &[
    "official",
    "video",
    "audio",
    "lyrics",
    "lyric",
    "music",
    "hd",
    "hq",
    "4k",
    "mv",
    "visualizer",
    "topic",
    "feat",
    "ft",
    "featuring",
    "remastered",
    "remaster",
    "version",
];

/// Words that make a different recording of the same song.
const VARIANTS: &[&str] = &[
    "live",
    "remix",
    "cover",
    "acoustic",
    "instrumental",
    "karaoke",
    "slowed",
    "reverb",
    "sped",
    "nightcore",
    "8d",
    "extended",
];

/// A search template filled in for `track`: `{isrc}`, `{title}`, `{artist}` (the first) and
/// `{artists}` (all of them). `None` for a template about an ISRC the track lacks.
pub(super) fn fill(template: &str, track: &TrackInfo) -> Option<String> {
    let isrc = match (&track.isrc, template.contains("{isrc}")) {
        (Some(isrc), _) => isrc.as_str(),
        (None, true) => return None,
        (None, false) => "",
    };
    let artists = track.author.as_deref().unwrap_or_default();
    let artist = artists.split(", ").next().unwrap_or_default();
    Some(
        template
            .replace("{isrc}", isrc)
            .replace("{title}", &track.title)
            .replace("{artists}", artists)
            .replace("{artist}", artist),
    )
}

/// How well `candidate` matches `wanted`, higher being better; `None` if it does not match.
pub(super) fn score(wanted: &TrackInfo, candidate: &TrackInfo) -> Option<f32> {
    let wanted_words = words(&wanted.title);
    let candidate_words = words(&candidate.title);
    let variant_of = |words: &[String]| -> Vec<&str> {
        VARIANTS
            .iter()
            .copied()
            .filter(|variant| words.iter().any(|w| w == variant))
            .collect()
    };
    if variant_of(&wanted_words) != variant_of(&candidate_words) {
        return None;
    }

    let length_fit = match (wanted.length, candidate.length) {
        (Some(Millis(a)), Some(Millis(b))) => {
            let difference = a.abs_diff(b);
            let tolerance = (a / 10).max(7_000);
            if difference > tolerance {
                return None;
            }
            Some(1.0 - difference as f32 / tolerance as f32)
        }
        _ => None,
    };

    let overlap = if wanted_words.is_empty() {
        0.0
    } else {
        let shared = wanted_words
            .iter()
            .filter(|w| candidate_words.contains(w))
            .count();
        shared as f32 / wanted_words.len() as f32
    };
    if overlap < 0.5 {
        return None;
    }

    let candidate_text = format!(
        "{} {}",
        words(candidate.author.as_deref().unwrap_or_default()).join(" "),
        candidate_words.join(" ")
    );
    let by_artist = wanted
        .author
        .as_deref()
        .unwrap_or_default()
        .split(", ")
        .map(|artist| words(artist).join(" "))
        .any(|artist| !artist.is_empty() && candidate_text.contains(&artist));

    Some(2.0 * overlap + f32::from(u8::from(by_artist)) + length_fit.unwrap_or(0.0))
}

/// Lowercase words of `text`, without noise words.
fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty() && !NOISE.contains(word))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str, author: &str, seconds: Option<u64>) -> TrackInfo {
        TrackInfo {
            identifier: title.into(),
            title: title.into(),
            author: Some(author.into()),
            author_url: None,
            album: None,
            length: seconds.map(|s| Millis(s * 1000)),
            is_stream: false,
            is_seekable: true,
            uri: String::new(),
            artwork_url: None,
            isrc: Some("GBAYE0800265".into()),
            source_name: "spotify".into(),
        }
    }

    #[test]
    fn fills_search_templates() {
        let wanted = track("Viva La Vida", "Coldplay, Someone", Some(242));
        assert_eq!(
            fill("ytsearch:\"{isrc}\"", &wanted).as_deref(),
            Some("ytsearch:\"GBAYE0800265\"")
        );
        assert_eq!(
            fill("ytmsearch:{artist} {title}", &wanted).as_deref(),
            Some("ytmsearch:Coldplay Viva La Vida")
        );
        assert_eq!(
            fill("{artists}", &wanted).as_deref(),
            Some("Coldplay, Someone")
        );
        let without_isrc = TrackInfo {
            isrc: None,
            ..wanted
        };
        assert_eq!(fill("ytsearch:\"{isrc}\"", &without_isrc), None);
    }

    #[test]
    fn prefers_the_same_recording() {
        let wanted = track("Viva La Vida - Remastered 2011", "Coldplay", Some(242));
        let topic = track("Viva La Vida", "Coldplay - Topic", Some(241));
        let video = track(
            "Coldplay - Viva La Vida (Official Video)",
            "Coldplay",
            Some(244),
        );
        let topic_score = score(&wanted, &topic).unwrap();
        let video_score = score(&wanted, &video).unwrap();
        assert!(topic_score > 2.5 && video_score > 2.5);

        let live = track("Viva La Vida (Live in Madrid)", "Coldplay", Some(250));
        assert_eq!(score(&wanted, &live), None, "a live recording");
        let long_cut = track("Viva La Vida", "Coldplay", Some(300));
        assert_eq!(score(&wanted, &long_cut), None, "a minute longer");
        // ISRC searches find unrelated videos too; the same length does not make it the song
        let other_song = track("Yellow Submarine", "The Beatles", Some(242));
        assert_eq!(score(&wanted, &other_song), None);
    }

    #[test]
    fn results_without_a_length_match_by_title() {
        let wanted = track("Yellow", "Coldplay", Some(266));
        let music_result = TrackInfo {
            author: None,
            ..track("Yellow", "", None)
        };
        assert!(score(&wanted, &music_result).is_some());
    }
}
