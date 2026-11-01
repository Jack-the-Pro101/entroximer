//! Runs a producer: demuxes its byte stream forward-only and pushes packets into the timeline.

use std::io;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::TryStreamExt;
use symphonia::core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::TimeBase;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

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
    let mss = MediaSourceStream::new(Box::new(source), Default::default());
    let mut probe_hint = Hint::new();
    if let Some(ext) = hint {
        probe_hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(
            &probe_hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(MediaError::Probe)?;
    let track = format
        .default_track(TrackType::Audio)
        .cloned()
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
    timeline.set_codec(&params)?;

    let opus = params.codec == CODEC_ID_OPUS;
    let offset = match timestamps {
        TimestampMode::Absolute => 0,
        TimestampMode::RelativeTo(offset) => offset,
    };
    let to_ticks = |ts: i64| -> Ticks {
        (ts as i128 * time_base.numer.get() as i128 * TICKS_PER_SECOND as i128
            / time_base.denom.get() as i128) as Ticks
    };

    let mut first = true;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => return Ok(Finish::Eof),
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(Finish::Eof);
            }
            Err(e) => return Err(MediaError::Demux(e)),
        };
        if packet.track_id != track.id {
            continue;
        }
        let container_dur = to_ticks(packet.dur.get() as i64);
        let dur = if opus {
            // container durations are often missing or rounded; the packet knows exactly
            opus2::packet::get_nb_samples(&packet.data, 48_000)
                .map_or(container_dur, |n| n as Ticks)
        } else {
            container_dur
        };
        // Opus pre-skip arrives as a negative timestamp on the first packet (Matroska
        // CodecDelay), which playback drops like any audio before the start position. symphonia
        // 0.6.1 does not yet turn Matroska DiscardPadding into `trim_end`, so up to one packet
        // of encoder padding is played at the very end of such tracks.
        let pkt = Packet {
            pts: to_ticks(packet.pts.get()) + offset,
            dur,
            trim_start: saturating_u32(packet.trim_start.get()),
            trim_end: saturating_u32(packet.trim_end.get()),
            data: Bytes::from(packet.data.into_vec()),
        };
        if first {
            tracing::debug!(producer, pts = pkt.pts, "first packet");
            first = false;
        }
        if let PushResult::Stop = timeline.push(producer, pkt) {
            return Ok(Finish::Stopped);
        }
    }
}

fn saturating_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}
