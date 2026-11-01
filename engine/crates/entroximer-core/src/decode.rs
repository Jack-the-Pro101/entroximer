//! Packet decoding to interleaved stereo f32.
//!
//! symphonia 0.6 has no Opus decoder (songbird's is built for symphonia 0.5), so Opus — YouTube's
//! best audio — is decoded with libopus via `opus2`; everything else uses symphonia.

use symphonia::core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::packet::PacketRef;
use symphonia::core::units::{Duration as SymphDuration, Timestamp};

use crate::dsp::CHANNELS;
use crate::timeline::{Packet, TICKS_PER_SECOND, Ticks};

/// Opus always decodes at 48 kHz.
const OPUS_SAMPLE_RATE: u32 = 48_000;
/// 120 ms, the longest Opus packet, at 48 kHz.
const OPUS_MAX_FRAMES: usize = 5760;

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("unsupported codec")]
    UnsupportedCodec(#[source] SymphoniaError),
    #[error("opus decoding failed")]
    Opus(#[from] opus2::Error),
    #[error("decoding failed")]
    Symphonia(#[source] SymphoniaError),
}

pub struct Decoder {
    kind: Kind,
    sample_rate: u32,
    scratch: Vec<f32>,
}

enum Kind {
    Opus(opus2::Decoder),
    Symphonia(Box<dyn AudioDecoder>),
}

impl std::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let backend = match self.kind {
            Kind::Opus(_) => "libopus",
            Kind::Symphonia(_) => "symphonia",
        };
        f.debug_struct("Decoder")
            .field("backend", &backend)
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

impl Decoder {
    pub fn new(params: &AudioCodecParameters) -> Result<Self, DecodeError> {
        if params.codec == CODEC_ID_OPUS {
            return Ok(Self {
                kind: Kind::Opus(opus2::Decoder::new(
                    OPUS_SAMPLE_RATE,
                    opus2::Channels::Stereo,
                )?),
                sample_rate: OPUS_SAMPLE_RATE,
                scratch: vec![0.0; OPUS_MAX_FRAMES * CHANNELS],
            });
        }
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(DecodeError::UnsupportedCodec)?;
        Ok(Self {
            kind: Kind::Symphonia(decoder),
            sample_rate: params.sample_rate.unwrap_or(OPUS_SAMPLE_RATE),
            scratch: Vec::new(),
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// How far before a seek target decoding should start for the output to be correct.
    pub fn preroll(params: &AudioCodecParameters) -> Ticks {
        if params.codec == CODEC_ID_OPUS {
            // RFC 7845 §4.6: decode at least 80 ms before the target
            TICKS_PER_SECOND * 80 / 1000
        } else {
            TICKS_PER_SECOND / 10
        }
    }

    /// Decodes one packet, appending interleaved stereo at `sample_rate()`; returns frames added.
    pub fn decode(&mut self, pkt: &Packet, out: &mut Vec<f32>) -> Result<usize, DecodeError> {
        match &mut self.kind {
            Kind::Opus(dec) => {
                let frames = dec.decode_float(&pkt.data, &mut self.scratch, false)?;
                let start = (pkt.trim_start as usize).min(frames);
                let end = frames.saturating_sub(pkt.trim_end as usize).max(start);
                out.extend_from_slice(&self.scratch[start * CHANNELS..end * CHANNELS]);
                Ok(end - start)
            }
            Kind::Symphonia(dec) => {
                let mut packet = PacketRef::new(
                    0,
                    Timestamp::new(pkt.pts),
                    SymphDuration::new(pkt.dur.max(0) as u64),
                    &pkt.data,
                );
                // symphonia's decoders apply the trims themselves
                packet.trim_start = SymphDuration::new(pkt.trim_start as u64);
                packet.trim_end = SymphDuration::new(pkt.trim_end as u64);
                let buf = dec.decode_ref(&packet).map_err(DecodeError::Symphonia)?;
                let channels = buf.spec().channels().count().max(1);
                let n = buf.samples_interleaved();
                self.scratch.resize(n, 0.0);
                buf.copy_to_slice_interleaved(&mut self.scratch[..n]);
                let frames = n / channels;
                match channels {
                    1 => out.extend(self.scratch[..n].iter().flat_map(|&s| [s, s])),
                    2 => out.extend_from_slice(&self.scratch[..n]),
                    _ => out.extend(
                        self.scratch[..n]
                            .chunks_exact(channels)
                            .flat_map(|f| [f[0], f[1]]),
                    ),
                }
                Ok(frames)
            }
        }
    }

    /// Forgets decoder state, e.g. after a seek.
    pub fn reset(&mut self) -> Result<(), DecodeError> {
        match &mut self.kind {
            Kind::Opus(dec) => dec.reset_state()?,
            Kind::Symphonia(dec) => dec.reset(),
        }
        Ok(())
    }
}
