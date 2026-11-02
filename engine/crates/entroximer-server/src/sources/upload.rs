//! `upload`: files clients uploaded to the engine (see [`crate::uploads`]), as `upload:<id>`.
//! They play straight from the file, with no ffmpeg in between, and their metadata is what the
//! upload's check read from the file's tags.

use std::io;
use std::sync::Arc;

use async_trait::async_trait;
use entroximer_core::producer::file::FileProducer;
use entroximer_core::producer::{Producer, ProducerCaps, ProducerStream};
use entroximer_core::timeline::Ticks;
use futures_util::future::BoxFuture;
use reqwest::Url;

use super::provider::{Provider, ProviderDescriptor};
use super::{FoundTrack, LoadOutcome, ResolveError, ResolvedTrack, TrackInfo, TrackRef};
use crate::api::protocol::Millis;
use crate::config::MediaContext;
use crate::uploads::{Lease, UploadFile, UploadStore};

/// The provider's name, and the scheme of its URIs.
pub const SOURCE: &str = "upload";
/// The title of an upload without one, given or tagged.
const UNTITLED: &str = "Uploaded audio";

pub(super) struct UploadProvider {
    descriptor: ProviderDescriptor,
    store: Arc<UploadStore>,
}

impl UploadProvider {
    pub(super) fn new(store: Arc<UploadStore>) -> Self {
        Self {
            descriptor: ProviderDescriptor {
                name: SOURCE.to_owned(),
                display_name: "Upload".to_owned(),
                search_prefixes: Vec::new(),
                hosts: Vec::new(),
            },
            store,
        }
    }

    fn play(&self, id: &str) -> Result<ResolvedTrack, ResolveError> {
        let lease = self
            .store
            .lease(id)
            .ok_or_else(|| ResolveError::UploadGone(id.to_owned()))?;
        Ok(ResolvedTrack {
            track: found_track(&lease),
            producer: Arc::new(UploadProducer(lease)),
        })
    }
}

#[async_trait]
impl Provider for UploadProvider {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    fn accepts(&self, url: &Url) -> bool {
        url.scheme() == SOURCE
    }

    async fn load_url(
        &self,
        url: &Url,
        _ctx: &MediaContext,
    ) -> Result<Option<LoadOutcome>, ResolveError> {
        self.play(url.path()).map(|t| Some(LoadOutcome::Track(t)))
    }

    async fn resolve(
        &self,
        track: &FoundTrack,
        _ctx: &MediaContext,
    ) -> Result<ResolvedTrack, ResolveError> {
        let uri = &track.reference.uri;
        let id = uri
            .strip_prefix(SOURCE)
            .and_then(|rest| rest.strip_prefix(':'))
            .ok_or_else(|| ResolveError::Unsupported(uri.clone()))?;
        self.play(id)
    }
}

/// An upload as a track.
pub fn found_track(file: &UploadFile) -> FoundTrack {
    let uri = format!("{SOURCE}:{}", file.id);
    let facts = &file.facts;
    FoundTrack {
        reference: TrackRef {
            source: SOURCE.to_owned(),
            uri: uri.clone(),
        },
        info: TrackInfo {
            identifier: file.id.clone(),
            title: file
                .title
                .clone()
                .or_else(|| facts.title.clone())
                .unwrap_or_else(|| UNTITLED.to_owned()),
            author: facts.artist.clone(),
            author_url: None,
            album: facts.album.clone(),
            length: Some(Millis::from_ticks(facts.length)).filter(|l| l.0 > 0),
            is_stream: false,
            is_seekable: true,
            uri,
            artwork_url: None,
            isrc: facts.isrc.clone(),
            source_name: SOURCE.to_owned(),
        },
    }
}

/// Reads the file, keeping it for as long as the track can play.
struct UploadProducer(Lease);

impl Producer for UploadProducer {
    fn caps(&self) -> ProducerCaps {
        ProducerCaps::default()
    }

    fn open(&self, start: Option<Ticks>) -> BoxFuture<'static, io::Result<ProducerStream>> {
        // the file has no extension, so the demuxer gets no hint, as when the upload was checked
        FileProducer {
            path: self.0.path().to_owned(),
        }
        .open(start)
    }
}
