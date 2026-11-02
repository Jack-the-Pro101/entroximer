//! Runs a producer: demuxes its byte stream forward-only and pushes packets into the timeline.
//! [`inspect`] reads media the same way to check that it plays.

use std::io;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::TryStreamExt;
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream, ReadOnlySource};
use symphonia::core::meta::{MetadataOptions, StandardTag};
use symphonia::core::units::TimeBase;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::decode::Decoder;
use crate::error::MediaError;
use crate::producer::{Producer, ProducerStream, TimestampMode};
use crate::timeline::{Finish, Packet, ProducerId, PushResult, TICKS_PER_SECOND, Ticks, Timeline};
use crate::transport::{StreamReader, stream_reader};

/// A running producer. Dropping it cancels the producer.
#[derive(Debug)]
pub struct DemuxTask {
    producer: ProducerId,
    timeline: Arc<Timeline>,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

impl DemuxTask {
    pub fn spawn(
        rt: &Handle,
        timeline: Arc<Timeline>,
        source: &dyn Producer,
        start: Option<Ticks>,
    ) -> Self {
        let producer = timeline.register_producer(start);
        let opening = source.open(start);
        let cancel = CancellationToken::new();
        tracing::debug!(producer, ?start, "opening producer");
        let handle = rt.spawn({
            let (timeline, cancel) = (timeline.clone(), cancel.clone());
            async move {
                let outcome = tokio::select! {
                    opened = opening => match opened {
                        Ok(stream) => run(timeline.clone(), producer, stream, cancel.clone()).await,
                        Err(e) => Finish::Failed(MediaError::Source(e)),
                    },
                    _ = cancel.cancelled() => Finish::Stopped,
                };
                let outcome = if cancel.is_cancelled() {
                    Finish::Stopped
                } else {
                    outcome
                };
                tracing::debug!(producer, ?outcome, "producer finished");
                timeline.finish(producer, outcome);
            }
        });
        Self {
            producer,
            timeline,
            cancel,
            handle,
        }
    }

    pub fn producer(&self) -> ProducerId {
        self.producer
    }

    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    pub fn cancel(&self) {
        self.timeline.cancel(self.producer);
        self.cancel.cancel();
    }
}

impl Drop for DemuxTask {
    fn drop(&mut self) {
        self.cancel();
    }
}

async fn run(
    timeline: Arc<Timeline>,
    producer: ProducerId,
    stream: ProducerStream,
    cancel: CancellationToken,
) -> Finish {
    // A failing producer usually makes the demuxer fail too, with a less useful error (a probe
    // that sees no data reports an unknown format), so keep the producer's own error.
    let source_failure = Arc::new(Mutex::new(None));
    let bytes = stream.bytes.map_err({
        let source_failure = Arc::clone(&source_failure);
        move |e| {
            let forwarded = io::Error::new(e.kind(), e.to_string());
            *source_failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
            forwarded
        }
    });
    let (download, source) = stream_reader(bytes);
    let (timestamps, hint) = (stream.timestamps, stream.hint);
    let demux = tokio::task::spawn_blocking(move || {
        demux_into(&timeline, producer, source, timestamps, hint.as_deref())
            .unwrap_or_else(Finish::Failed)
    });
    // closing the byte channel unblocks the demuxer; the demux result is what matters
    let download = async {
        tokio::select! {
            _ = download => {}
            _ = cancel.cancelled() => {}
        }
    };
    let ((), demuxed) = futures_util::future::join(download, demux).await;
    match demuxed.unwrap_or(Finish::Failed(MediaError::Panicked)) {
        Finish::Failed(demux_error) => {
            let source_error = source_failure
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
            Finish::Failed(source_error.map_or(demux_error, MediaError::Source))
        }
        finish => finish,
    }
}

fn demux_into(
    timeline: &Timeline,
    producer: ProducerId,
    source: StreamReader,
    timestamps: TimestampMode,
    hint: Option<&str>,
) -> Result<Finish, MediaError> {
    let offset = match timestamps {
        TimestampMode::Absolute => 0,
        TimestampMode::RelativeTo(offset) => offset,
    };
    let mut demuxer = AudioDemuxer::open(Box::new(source), hint, offset)?;
    timeline.set_codec(&demuxer.params)?;
    let mut first = true;
    while let Some(pkt) = demuxer.next_packet()? {
        if first {
            tracing::debug!(producer, pts = pkt.pts, "first packet");
            first = false;
        }
        if let PushResult::Stop = timeline.push(producer, pkt) {
            return Ok(Finish::Stopped);
        }
    }
    Ok(Finish::Eof)
}

/// What a media file holds, as [`inspect`] finds it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MediaFacts {
    /// Up to the end of the last audio packet.
    pub length: Ticks,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub isrc: Option<String>,
    /// Whether it embeds a picture, such as an album cover.
    pub has_cover: bool,
}

/// Packets decoded by [`inspect`], of which one must decode.
const INSPECTED_PACKETS: usize = 8;

/// Reads media front to back the way a producer's stream with the same `hint` (a file extension)
/// is played: unseekable. It decodes the first packets, so that media it accepts is media the
/// engine plays, and reads the rest for the length and tags. Blocks while reading.
pub fn inspect(
    source: impl io::Read + Send + Sync + 'static,
    hint: Option<&str>,
) -> Result<MediaFacts, MediaError> {
    let mut demuxer = AudioDemuxer::open(Box::new(ReadOnlySource::new(source)), hint, 0)?;
    let mut decoder = Decoder::new(&demuxer.params).map_err(MediaError::Decode)?;
    let mut facts = MediaFacts::default();
    let (mut decoded, mut tried, mut last_error) = (false, 0, None);
    let mut pcm = Vec::new();
    while let Some(pkt) = demuxer.next_packet()? {
        facts.length = facts.length.max(pkt.pts + pkt.dur);
        if !decoded && tried < INSPECTED_PACKETS {
            tried += 1;
            pcm.clear();
            match decoder.decode(&pkt, &mut pcm) {
                Ok(_) => decoded = true,
                Err(e) => last_error = Some(e),
            }
        }
    }
    match last_error {
        Some(e) if !decoded => return Err(MediaError::Decode(e)),
        _ if !decoded => return Err(MediaError::NoAudioTrack),
        _ => {}
    }
    demuxer.read_tags(&mut facts);
    Ok(facts)
}

/// A container opened for its default audio track, read packet by packet.
struct AudioDemuxer {
    format: Box<dyn FormatReader>,
    track_id: u32,
    params: AudioCodecParameters,
    time_base: TimeBase,
    opus: bool,
    /// Added to every timestamp.
    offset: Ticks,
}

impl AudioDemuxer {
    fn open(
        source: Box<dyn MediaSource>,
        hint: Option<&str>,
        offset: Ticks,
    ) -> Result<Self, MediaError> {
        let mss = MediaSourceStream::new(source, Default::default());
        let mut probe_hint = Hint::new();
        if let Some(ext) = hint {
            probe_hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe()
            .probe(
                &probe_hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(MediaError::Probe)?;
        let track = format
            .default_track(TrackType::Audio)
            .ok_or(MediaError::NoAudioTrack)?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .cloned()
            .ok_or(MediaError::IncompleteTrack("codec parameters"))?;
        let time_base = track
            .time_base
            .or_else(|| params.sample_rate.and_then(TimeBase::try_from_recip))
            .ok_or(MediaError::IncompleteTrack("time base"))?;
        Ok(Self {
            track_id: track.id,
            opus: params.codec == CODEC_ID_OPUS,
            format,
            params,
            time_base,
            offset,
        })
    }

    fn to_ticks(&self, ts: i64) -> Ticks {
        let base = self.time_base;
        (ts as i128 * base.numer.get() as i128 * TICKS_PER_SECOND as i128
            / base.denom.get() as i128) as Ticks
    }

    /// The audio track's next packet, or `None` at the end of the media.
    fn next_packet(&mut self) -> Result<Option<Packet>, MediaError> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => return Ok(None),
                Err(SymphoniaError::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(MediaError::Demux(e)),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let container_dur = self.to_ticks(packet.dur.get() as i64);
            let dur = if self.opus {
                // container durations are often missing or rounded; the packet knows exactly
                opus2::packet::get_nb_samples(&packet.data, 48_000)
                    .map_or(container_dur, |n| n as Ticks)
            } else {
                container_dur
            };
            // Opus pre-skip arrives as a negative timestamp on the first packet (Matroska
            // CodecDelay), which playback drops like any audio before the start position.
            // symphonia 0.6.1 does not yet turn Matroska DiscardPadding into `trim_end`, so up to
            // one packet of encoder padding is played at the very end of such tracks.
            return Ok(Some(Packet {
                pts: self.to_ticks(packet.pts.get()) + self.offset,
                dur,
                trim_start: saturating_u32(packet.trim_start.get()),
                trim_end: saturating_u32(packet.trim_end.get()),
                data: Bytes::from(packet.data.into_vec()),
            }));
        }
    }

    /// Fills in the facts' tags and cover from the media's and the audio track's metadata, the
    /// first value found for each.
    fn read_tags(&mut self, facts: &mut MediaFacts) {
        let track_id = u64::from(self.track_id);
        let mut metadata = self.format.metadata();
        loop {
            if let Some(revision) = metadata.current() {
                let per_track = revision
                    .per_track
                    .iter()
                    .filter(|t| t.track_id == track_id)
                    .map(|t| &t.metadata);
                let containers: Vec<_> =
                    std::iter::once(&revision.media).chain(per_track).collect();
                facts.has_cover |= containers.iter().any(|m| !m.visuals.is_empty());
                for tag in containers.iter().flat_map(|m| &m.tags) {
                    let (field, value) = match &tag.std {
                        Some(StandardTag::TrackTitle(v)) => (&mut facts.title, v),
                        Some(StandardTag::Artist(v) | StandardTag::AlbumArtist(v)) => {
                            (&mut facts.artist, v)
                        }
                        Some(StandardTag::Album(v)) => (&mut facts.album, v),
                        Some(StandardTag::IdentIsrc(v)) => (&mut facts.isrc, v),
                        _ => continue,
                    };
                    let value = value.trim();
                    if field.is_none() && !value.is_empty() {
                        *field = Some(value.to_owned());
                    }
                }
            }
            if metadata.pop().is_none() {
                return;
            }
        }
    }
}

fn saturating_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}
