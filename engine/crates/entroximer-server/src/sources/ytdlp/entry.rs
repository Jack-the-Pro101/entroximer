//! yt-dlp's info dicts (single videos, and the entries of playlists and searches) and their
//! translation into the common metadata model.

use serde::Deserialize;

use super::profiles::Site;
use crate::api::protocol::Millis;
use crate::sources::{FoundTrack, TrackInfo, TrackRef};

/// The fields of an info dict the translation uses; everything else is ignored.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Entry {
    #[serde(rename = "_type")]
    pub(crate) kind: Option<String>,
    pub(crate) id: Option<String>,
    pub(crate) title: Option<String>,
    /// The song title, on music sites.
    pub(crate) track: Option<String>,
    pub(crate) artists: Option<Vec<String>>,
    pub(crate) artist: Option<String>,
    pub(crate) creators: Option<Vec<String>>,
    pub(crate) creator: Option<String>,
    pub(crate) uploader: Option<String>,
    pub(crate) channel: Option<String>,
    pub(crate) uploader_url: Option<String>,
    pub(crate) channel_url: Option<String>,
    pub(crate) album: Option<String>,
    pub(crate) duration: Option<f64>,
    pub(crate) is_live: Option<bool>,
    pub(crate) live_status: Option<String>,
    pub(crate) webpage_url: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) thumbnail: Option<String>,
    pub(crate) thumbnails: Vec<Thumbnail>,
    pub(crate) extractor_key: Option<String>,
    pub(crate) ie_key: Option<String>,
    pub(crate) playlist_count: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Thumbnail {
    pub(crate) url: String,
    pub(crate) width: Option<u32>,
    pub(crate) height: Option<u32>,
    pub(crate) preference: Option<i64>,
}

impl Entry {
    pub(crate) fn extractor(&self) -> Option<&str> {
        self.extractor_key.as_deref().or(self.ie_key.as_deref())
    }

    pub(crate) fn is_live(&self) -> bool {
        self.is_live == Some(true)
            || matches!(self.live_status.as_deref(), Some("is_live" | "is_upcoming"))
    }

    /// A playlist or channel tab listed inside a playlist, rather than a track.
    pub(crate) fn is_collection(&self) -> bool {
        self.kind.as_deref() == Some("playlist")
            || self.extractor().is_some_and(|key| key.ends_with("Tab"))
    }

    /// The page people would open; flat SoundCloud entries' `url` is an API link.
    pub(crate) fn uri(&self) -> Option<&str> {
        nonempty(&self.webpage_url).or(nonempty(&self.url))
    }

    pub(crate) fn author(&self) -> Option<String> {
        let first = |names: &Option<Vec<String>>| {
            names
                .as_ref()
                .and_then(|names| names.first())
                .filter(|name| !name.trim().is_empty())
                .cloned()
        };
        first(&self.artists)
            .or_else(|| nonempty(&self.artist).map(str::to_owned))
            .or_else(|| first(&self.creators))
            .or_else(|| nonempty(&self.creator).map(str::to_owned))
            .or_else(|| nonempty(&self.uploader).map(str::to_owned))
            .or_else(|| nonempty(&self.channel).map(str::to_owned))
    }

    /// yt-dlp's choice of thumbnail, else the largest (or most preferred) one listed.
    pub(crate) fn artwork(&self) -> Option<String> {
        if let Some(thumbnail) = nonempty(&self.thumbnail) {
            return Some(thumbnail.to_owned());
        }
        self.thumbnails
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.url.is_empty())
            .max_by_key(|(position, t)| {
                let area = u64::from(t.width.unwrap_or(0)) * u64::from(t.height.unwrap_or(0));
                (area, t.preference.unwrap_or(i64::MIN), *position)
            })
            .map(|(_, t)| t.url.clone())
    }

    /// The track this entry describes, named for `site`; `None` without a link to it.
    pub(crate) fn found_track(&self, site: &Site) -> Option<FoundTrack> {
        let uri = self.uri()?.to_owned();
        let live = self.is_live();
        let mut info = TrackInfo {
            identifier: self.id.clone().unwrap_or_else(|| uri.clone()),
            title: nonempty(&self.track)
                .or(nonempty(&self.title))
                .unwrap_or("Unknown title")
                .to_owned(),
            author: self.author(),
            author_url: nonempty(&self.uploader_url)
                .or(nonempty(&self.channel_url))
                .map(str::to_owned),
            album: nonempty(&self.album).map(str::to_owned),
            length: self
                .duration
                .filter(|_| !live)
                .and_then(Millis::from_secs_f64),
            is_stream: live,
            is_seekable: !live,
            uri: uri.clone(),
            artwork_url: self.artwork(),
            isrc: None,
            source_name: site.name.to_owned(),
        };
        (site.adjust)(&mut info, self);
        Some(FoundTrack {
            reference: TrackRef {
                source: site.name.to_owned(),
                uri,
            },
            info,
        })
    }
}

fn nonempty(text: &Option<String>) -> Option<&str> {
    text.as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::sources::ytdlp::profiles::{SOUNDCLOUD, YOUTUBE};

    fn first_entry(json: &str) -> Entry {
        let json: Value = serde_json::from_str(json).unwrap();
        let entry = json.get("entries").map_or(&json, |entries| &entries[0]);
        Entry::deserialize(entry).unwrap()
    }

    #[test]
    fn youtube_search_results_are_complete() {
        let entry = first_entry(include_str!(
            "../../../tests/fixtures/ytdlp/youtube_search.json"
        ));
        let track = entry.found_track(&YOUTUBE).unwrap();
        assert_eq!(track.reference.source, "youtube");
        assert_eq!(
            track.info.title,
            "Fallen Kingdom: The Complete Minecraft Music Video Series"
        );
        assert_eq!(track.info.author.as_deref(), Some("CaptainSparklez"));
        assert_eq!(
            track.info.author_url.as_deref(),
            Some("https://www.youtube.com/@CaptainSparklez")
        );
        assert_eq!(track.info.length, Some(Millis(1_345_000)));
        assert_eq!(
            track.info.uri,
            "https://www.youtube.com/watch?v=ayl3UXKpH1g"
        );
        assert!(
            track
                .info
                .artwork_url
                .unwrap()
                .starts_with("https://i.ytimg.com/vi/ayl3UXKpH1g/")
        );
    }

    #[test]
    fn youtube_music_results_get_artwork_but_wait_for_the_rest() {
        let entry = first_entry(include_str!(
            "../../../tests/fixtures/ytdlp/youtube_music_search.json"
        ));
        let info = entry.found_track(&YOUTUBE).unwrap().info;
        assert_eq!(info.title, "Viva La Vida");
        assert_eq!(info.author, None, "known only once the track is resolved");
        assert_eq!(info.length, None);
        assert_eq!(info.uri, "https://music.youtube.com/watch?v=ALsvdSA9tOU");
        assert_eq!(
            info.artwork_url.as_deref(),
            Some("https://i.ytimg.com/vi/ALsvdSA9tOU/hqdefault.jpg")
        );
    }

    #[test]
    fn soundcloud_results_link_to_pages_and_moderate_artwork() {
        for json in [
            include_str!("../../../tests/fixtures/ytdlp/soundcloud_search.json"),
            include_str!("../../../tests/fixtures/ytdlp/soundcloud_track.json"),
        ] {
            let info = first_entry(json).found_track(&SOUNDCLOUD).unwrap().info;
            assert_eq!(info.title, "Fallen Kingdom - A Minecraft Parody");
            assert_eq!(info.author.as_deref(), Some("Aether"));
            assert_eq!(info.length, Some(Millis(287_735)));
            assert_eq!(
                info.uri, "https://soundcloud.com/clement-jonathan/fallen-kingdom-a-minecraft",
                "never the API link"
            );
            let artwork = info.artwork_url.unwrap();
            assert!(artwork.contains("-t500x500."), "{artwork}");
        }
    }

    #[test]
    fn full_extractions_use_yt_dlps_thumbnail() {
        let entry = first_entry(include_str!(
            "../../../tests/fixtures/ytdlp/youtube_video.json"
        ));
        let info = entry.found_track(&YOUTUBE).unwrap().info;
        assert!(info.title.starts_with("\"Fallen Kingdom\""));
        assert_eq!(info.length, Some(Millis(288_000)));
        assert_eq!(
            info.artwork_url.as_deref(),
            Some("https://i.ytimg.com/vi/I-sH53vXP2A/hqdefault.jpg")
        );
        assert!(!info.is_stream && info.is_seekable);
    }

    #[test]
    fn music_metadata_wins_over_upload_metadata() {
        let entry = Entry {
            title: Some("Coldplay - Viva La Vida (Official Video)".into()),
            track: Some("Viva La Vida".into()),
            artists: Some(vec!["Coldplay".into()]),
            uploader: Some("ColdplayVEVO".into()),
            webpage_url: Some("https://www.youtube.com/watch?v=x".into()),
            ..Entry::default()
        };
        let info = entry.found_track(&YOUTUBE).unwrap().info;
        assert_eq!(info.title, "Viva La Vida");
        assert_eq!(info.author.as_deref(), Some("Coldplay"));
    }
}
