//! `local`: files on the engine's host, if the operator allows them, either anywhere the engine
//! can read (`ENTROXIMER_ALLOW_LOCAL_FILES`) or only within one directory
//! (`ENTROXIMER_LOCAL_FILES_DIR`), e.g. a bot's sounds on a volume it shares with the engine.
//!
//! Files of up to [`DIRECT_MAX_BYTES`] that the engine's demuxer reads by itself play straight
//! from the file, like uploads, without an ffprobe and an ffmpeg process each time: what such a
//! file holds is read once, and again only when the file changes. This suits short sounds played
//! often. Other files play through ffmpeg, as `http` links do.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use async_trait::async_trait;
use entroximer_core::demux::{MediaFacts, inspect};
use entroximer_core::producer::file::FileProducer;
use reqwest::Url;

use super::direct::{describe_file, descriptor, title_from};
use super::provider::{Provider, ProviderDescriptor};
use super::{
    FoundTrack, Listing, LoadOutcome, ResolveError, ResolvedTrack, TrackInfo, TrackRef, artwork,
};
use crate::api::protocol::Millis;
use crate::config::{MediaContext, SourcesConfig};
use crate::expiring::Expiring;

pub const SOURCE: &str = "local";
/// Larger files play through ffmpeg: reading one through to learn its length would take long.
const DIRECT_MAX_BYTES: u64 = 64 << 20;
/// What files hold is kept this long after it was read, unless they change.
const KEEP_FOR: Duration = Duration::from_secs(60 * 60);
const CACHE_CAPACITY: usize = 1024;

/// Which files on the engine's host clients may play.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalFiles {
    Off,
    /// Any file the engine can read.
    Anywhere,
    /// Files within this directory, which is canonical.
    Within(PathBuf),
}

impl LocalFiles {
    pub fn new(config: &SourcesConfig) -> anyhow::Result<Self> {
        // an empty variable, e.g. from `${ENTROXIMER_LOCAL_FILES_DIR:-}` in compose files, means none
        let dir = config
            .local_files_dir
            .as_ref()
            .filter(|dir| !dir.as_os_str().is_empty());
        match dir {
            Some(dir) => {
                let canonical = std::fs::canonicalize(dir).with_context(|| {
                    format!("cannot use {} as the local files directory", dir.display())
                })?;
                anyhow::ensure!(canonical.is_dir(), "{} is not a directory", dir.display());
                Ok(Self::Within(canonical))
            }
            None if config.allow_local_files => Ok(Self::Anywhere),
            None => Ok(Self::Off),
        }
    }

    pub fn is_on(&self) -> bool {
        *self != Self::Off
    }

    /// The file `path` names, canonical, if clients may play it. With a directory, relative
    /// paths are within it, and paths that lead out of it (`..`, links) are refused.
    pub fn permit(&self, path: &Path) -> Option<PathBuf> {
        let file = match self {
            Self::Off => return None,
            Self::Anywhere => std::fs::canonicalize(path).ok()?,
            Self::Within(dir) => std::fs::canonicalize(dir.join(path))
                .ok()
                .filter(|file| file.starts_with(dir))?,
        };
        file.is_file().then_some(file)
    }
}

/// What a file held when it had this size and modification time.
type FileKey = (PathBuf, u64, Option<SystemTime>);

pub(super) struct LocalProvider {
    descriptor: ProviderDescriptor,
    files: LocalFiles,
    /// `None` for files the engine's demuxer does not read.
    inspected: Expiring<FileKey, Option<Arc<MediaFacts>>>,
}

impl LocalProvider {
    pub(super) fn new(files: LocalFiles) -> Self {
        Self {
            descriptor: descriptor(SOURCE, "Local file"),
            files,
            inspected: Expiring::new(KEEP_FOR, CACHE_CAPACITY),
        }
    }

    async fn play(&self, path: &Path, ctx: &MediaContext) -> Result<ResolvedTrack, ResolveError> {
        let file = self
            .files
            .permit(path)
            .ok_or_else(|| ResolveError::NoLocalFile(path.display().to_string()))?;
        let Some(facts) = self.inspect(&file).await else {
            return describe_file(&file, SOURCE, ctx).await;
        };
        Ok(ResolvedTrack {
            track: found_track(&file, &facts),
            producer: Arc::new(FileProducer { path: file }),
        })
    }

    /// What `file` holds, if it plays straight from the file.
    async fn inspect(&self, file: &Path) -> Option<Arc<MediaFacts>> {
        let metadata = tokio::fs::metadata(file).await.ok()?;
        if metadata.len() > DIRECT_MAX_BYTES {
            return None;
        }
        let key = (file.to_owned(), metadata.len(), metadata.modified().ok());
        if let Some(known) = self.inspected.get(&key) {
            return known;
        }
        let reading = file.to_owned();
        let facts = tokio::task::spawn_blocking(move || {
            let hint = extension(&reading);
            let source = std::fs::File::open(&reading).ok()?;
            // with the hint `FileProducer` gives, so that it plays as it was read
            inspect(source, hint.as_deref())
                .inspect_err(|e| {
                    tracing::debug!(
                        file = %reading.display(),
                        error = %entroximer_core::error::display_chain(e),
                        "plays through ffmpeg"
                    );
                })
                .ok()
        })
        .await
        .ok()
        .flatten()
        .map(Arc::new);
        self.inspected.insert(key, facts.clone());
        facts
    }
}

#[async_trait]
impl Provider for LocalProvider {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    fn accepts(&self, url: &Url) -> bool {
        url.scheme() == "file"
    }

    async fn load_url(
        &self,
        url: &Url,
        _listing: Listing,
        ctx: &MediaContext,
    ) -> Result<Option<LoadOutcome>, ResolveError> {
        let path = url
            .to_file_path()
            .map_err(|()| ResolveError::Unsupported(url.to_string()))?;
        let track = self.play(&path, ctx).await?;
        Ok(Some(LoadOutcome::Track(track)))
    }

    async fn resolve(
        &self,
        track: &FoundTrack,
        ctx: &MediaContext,
    ) -> Result<ResolvedTrack, ResolveError> {
        self.play(Path::new(&track.reference.uri), ctx).await
    }
}

fn extension(file: &Path) -> Option<String> {
    file.extension()
        .map(|extension| extension.to_string_lossy().into_owned())
}

/// A file that plays straight from disk, as a track.
fn found_track(file: &Path, facts: &MediaFacts) -> FoundTrack {
    let uri = file.to_string_lossy().into_owned();
    let reference = TrackRef {
        source: SOURCE.to_owned(),
        uri: uri.clone(),
    };
    let info = TrackInfo {
        identifier: uri.clone(),
        title: facts.title.clone().unwrap_or_else(|| title_from(&uri)),
        author: facts.artist.clone(),
        author_url: None,
        album: facts.album.clone(),
        length: Some(Millis::from_ticks(facts.length)).filter(|length| length.0 > 0),
        is_stream: false,
        is_seekable: true,
        uri,
        artwork_url: facts.has_cover.then(|| artwork::cover_path(&reference)),
        isrc: facts.isrc.clone(),
        source_name: SOURCE.to_owned(),
    };
    FoundTrack { reference, info }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::config::MediaConfig;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        media: MediaConfig,
    }

    /// Makes `name` in `dir` with ffmpeg from a lavfi `input` and output `args`.
    fn make(dir: &Path, name: &str, input: &str, args: &[&str]) -> PathBuf {
        let path = dir.join(name);
        let status = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-i", input])
            .args(args)
            .arg(&path)
            .status()
            .expect("ffmpeg must be installed to run these tests");
        assert!(status.success());
        path
    }

    fn sine(seconds: u32) -> String {
        format!("sine=frequency=440:sample_rate=48000:duration={seconds}")
    }

    #[test]
    fn a_directory_confines_local_files_to_it() {
        let root = tempfile::tempdir().unwrap();
        let sounds = root.path().join("sounds");
        std::fs::create_dir_all(sounds.join("bells")).unwrap();
        let (inside, outside) = (
            sounds.join("bells/ding.ogg"),
            root.path().join("secret.ogg"),
        );
        std::fs::write(&inside, b"").unwrap();
        std::fs::write(&outside, b"").unwrap();
        std::os::unix::fs::symlink(&outside, sounds.join("link.ogg")).unwrap();

        let config = |dir: Option<&Path>, anywhere: bool| SourcesConfig {
            default_search: "ytsearch".into(),
            playlist_limit: 1,
            allow_local_files: anywhere,
            local_files_dir: dir.map(Path::to_owned),
            allow_private_urls: false,
        };
        let within = LocalFiles::new(&config(Some(&sounds), false)).unwrap();
        let inside = std::fs::canonicalize(&inside).unwrap();
        assert_eq!(within.permit(&inside), Some(inside.clone()));
        assert_eq!(
            within.permit(Path::new("bells/ding.ogg")),
            Some(inside.clone())
        );
        for refused in [
            outside.as_path(),
            Path::new("../secret.ogg"),
            Path::new("link.ogg"),
            Path::new("bells"),
            Path::new("missing.ogg"),
        ] {
            assert_eq!(within.permit(refused), None, "{}", refused.display());
        }

        let anywhere = LocalFiles::new(&config(None, true)).unwrap();
        assert!(anywhere.permit(&outside).is_some());
        let off = LocalFiles::new(&config(None, false)).unwrap();
        assert_eq!(off.permit(&inside), None);
        assert!(LocalFiles::new(&config(Some(&root.path().join("nope")), false)).is_err());
    }

    #[tokio::test]
    async fn short_files_the_demuxer_reads_play_straight_from_disk() {
        let root = tempfile::tempdir().unwrap();
        let path = |name: &str| root.path().join(name).to_string_lossy().into_owned();
        let (sounds, data, cache) = (path("sounds"), path("data"), path("cache"));
        std::fs::create_dir(&sounds).unwrap();
        let args = ["entroximer", "--data-dir", &data, "--cache-dir", &cache];
        let ctx = MediaContext::new(Cli::parse_from(args).media).unwrap();
        let sounds = std::fs::canonicalize(&sounds).unwrap();
        let provider = LocalProvider::new(LocalFiles::Within(sounds.clone()));

        let ding = make(&sounds, "ding.ogg", &sine(1), &["-c:a", "libopus"]);
        let played = provider.play(&ding, &ctx).await.unwrap();
        assert!(
            !played.producer.caps().time_seek,
            "read by the engine itself"
        );
        let info = &played.track.info;
        assert_eq!(
            (info.title.as_str(), info.source_name.as_str()),
            ("ding", SOURCE)
        );
        assert!(
            info.length.unwrap().0.abs_diff(1000) < 25,
            "{:?}",
            info.length
        );

        // changed, it is read again
        make(&sounds, "ding.ogg", &sine(2), &["-c:a", "libopus"]);
        let played = provider.play(&ding, &ctx).await.unwrap();
        assert!(played.track.info.length.unwrap().0.abs_diff(2000) < 25);

        let covered = sounds.join("covered.mp3");
        let status = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-i", &sine(1)])
            .args(["-f", "lavfi", "-i", "color=c=red:s=64x64:d=1"])
            .args(["-map", "0:a", "-map", "1:v", "-frames:v", "1"])
            .args([
                "-c:a",
                "libmp3lame",
                "-c:v",
                "mjpeg",
                "-disposition:v",
                "attached_pic",
            ])
            .arg(&covered)
            .status()
            .unwrap();
        assert!(status.success());
        let played = provider.play(&covered, &ctx).await.unwrap();
        assert!(!played.producer.caps().time_seek);
        assert!(
            played.track.info.artwork_url.is_some(),
            "its cover is found"
        );

        // the engine's demuxer cannot read an MP4 whose index is at its end without seeking
        let m4a = make(&sounds, "chime.m4a", &sine(1), &["-c:a", "aac"]);
        let played = provider.play(&m4a, &ctx).await.unwrap();
        assert!(played.producer.caps().time_seek, "played through ffmpeg");

        let outside = make(root.path(), "elsewhere.ogg", &sine(1), &["-c:a", "libopus"]);
        let refused = provider.play(&outside, &ctx).await.unwrap_err();
        assert!(
            matches!(refused, ResolveError::NoLocalFile(_)),
            "{refused:?}"
        );
    }
}
