//! The sites the engine knows by name, all handled by yt-dlp: which links are theirs, how to
//! search them, and how their metadata needs adjusting. Links to other sites go to yt-dlp's
//! generic extractor.

use reqwest::Url;

use super::entry::Entry;
use crate::sources::TrackInfo;

pub(crate) struct Site {
    pub(crate) name: &'static str,
    pub(crate) display_name: &'static str,
    /// Hosts whose links belong to the site, subdomains included.
    pub(crate) hosts: &'static [&'static str],
    pub(crate) searches: &'static [Search],
    /// Site-specific fixes after the common translation.
    pub(crate) adjust: fn(&mut TrackInfo, &Entry),
}

pub(crate) struct Search {
    /// What users write before the terms, as in `ytsearch:terms`.
    pub(crate) prefix: &'static str,
    pub(crate) target: SearchTarget,
}

pub(crate) enum SearchTarget {
    /// yt-dlp's own search, e.g. `ytsearch10:terms`.
    Key(&'static str),
    /// A search page whose results yt-dlp lists.
    Page(fn(&str) -> String),
}

pub(crate) static YOUTUBE: Site = Site {
    name: "youtube",
    display_name: "YouTube",
    hosts: &["youtube.com", "youtu.be", "youtube-nocookie.com"],
    searches: &[
        Search {
            prefix: "ytsearch",
            target: SearchTarget::Key("ytsearch"),
        },
        Search {
            prefix: "ytmsearch",
            target: SearchTarget::Page(youtube_music_search),
        },
    ],
    adjust: adjust_youtube,
};

pub(crate) static SOUNDCLOUD: Site = Site {
    name: "soundcloud",
    display_name: "SoundCloud",
    hosts: &["soundcloud.com", "snd.sc"],
    searches: &[Search {
        prefix: "scsearch",
        target: SearchTarget::Key("scsearch"),
    }],
    adjust: adjust_soundcloud,
};

pub(crate) static NEWGROUNDS: Site = Site {
    name: "newgrounds",
    display_name: "Newgrounds",
    hosts: &["newgrounds.com"],
    searches: &[],
    adjust: keep,
};

pub(crate) static BANDCAMP: Site = Site {
    name: "bandcamp",
    display_name: "Bandcamp",
    hosts: &["bandcamp.com"],
    searches: &[],
    adjust: keep,
};

/// Whether yt-dlp run on `target` talks to YouTube: a YouTube link, a YouTube Music search
/// page, or a `ytsearch` key.
pub(crate) fn is_youtube_target(target: &str) -> bool {
    target.starts_with("ytsearch")
        || Url::parse(target)
            .ok()
            .and_then(|url| url.host_str().map(|host| YOUTUBE.claims(host)))
            .unwrap_or(false)
}

/// Any other site yt-dlp supports.
pub(crate) static GENERIC: Site = Site {
    name: "generic",
    display_name: "Other sites (yt-dlp)",
    hosts: &[],
    searches: &[],
    adjust: adjust_generic,
};

/// The named sites, in the order links are matched against them.
pub(crate) static SITES: [&Site; 4] = [&YOUTUBE, &SOUNDCLOUD, &NEWGROUNDS, &BANDCAMP];

impl Site {
    pub(crate) fn claims(&self, host: &str) -> bool {
        self.hosts.iter().any(|known| {
            host == *known
                || host
                    .strip_suffix(known)
                    .is_some_and(|sub| sub.ends_with('.'))
        })
    }
}

/// The named site a link belongs to.
pub(crate) fn site_of(uri: &str) -> Option<&'static Site> {
    let url = Url::parse(uri).ok()?;
    let host = url.host_str()?;
    SITES.iter().copied().find(|site| site.claims(host))
}

fn youtube_music_search(terms: &str) -> String {
    let mut url = Url::parse_with_params("https://music.youtube.com/search", [("q", terms)])
        .expect("the search URL is valid");
    url.set_fragment(Some("songs"));
    url.into()
}

/// YouTube thumbnails by video ID. Every video has this one, and it stays the same whether the
/// video was found by a search, listed in a playlist or loaded to play, where yt-dlp reports a
/// different signed URL each time (YouTube Music results: none), so a queued track's artwork
/// does not change when it is resolved.
fn adjust_youtube(info: &mut TrackInfo, entry: &Entry) {
    if let Some(id) = entry.id.as_deref().filter(|id| is_video_id(id)) {
        info.artwork_url = Some(format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"));
    }
}

fn is_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The original artwork can be several megabytes; 500×500 is plenty for an embed.
fn adjust_soundcloud(info: &mut TrackInfo, _: &Entry) {
    if let Some(artwork) = &mut info.artwork_url {
        *artwork = artwork.replace("-original.", "-t500x500.");
    }
}

/// Tracks of other sites are named after yt-dlp's extractor, e.g. `vimeo`.
fn adjust_generic(info: &mut TrackInfo, entry: &Entry) {
    if let Some(extractor) = entry.extractor() {
        info.source_name = extractor.to_lowercase();
    }
}

fn keep(_: &mut TrackInfo, _: &Entry) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_youtube_targets() {
        for target in [
            "https://www.youtube.com/watch?v=x",
            "https://youtu.be/x",
            "https://music.youtube.com/search?q=x",
            "ytsearch5:terms",
        ] {
            assert!(is_youtube_target(target), "{target}");
        }
        for target in [
            "https://soundcloud.com/a/b",
            "scsearch5:terms",
            "https://notyoutube.com/x",
            "https://example.com/youtube.com",
        ] {
            assert!(!is_youtube_target(target), "{target}");
        }
    }

    #[test]
    fn links_are_matched_to_sites_by_host() {
        let name = |uri: &str| site_of(uri).map(|site| site.name);
        assert_eq!(name("https://www.youtube.com/watch?v=x"), Some("youtube"));
        assert_eq!(name("https://music.youtube.com/watch?v=x"), Some("youtube"));
        assert_eq!(name("https://youtu.be/x"), Some("youtube"));
        assert_eq!(name("https://soundcloud.com/a/b"), Some("soundcloud"));
        assert_eq!(
            name("https://artist.bandcamp.com/track/x"),
            Some("bandcamp")
        );
        assert_eq!(
            name("https://notyoutube.com/watch"),
            None,
            "suffixes match whole labels"
        );
        assert_eq!(name("https://vimeo.com/1"), None);
    }

    #[test]
    fn youtube_music_searches_the_songs_section() {
        assert_eq!(
            youtube_music_search("viva la vida & more"),
            "https://music.youtube.com/search?q=viva+la+vida+%26+more#songs"
        );
    }
}
