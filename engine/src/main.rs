mod web_media_source;

use std::fs;
use std::io;

use fundsp::{
    prelude::{AudioNode, AudioUnit, bell_hz, db_amp},
    prelude32::reverb_stereo,
};
use futures_util::StreamExt;
use futures_util::TryStreamExt;
use hound::{WavSpec, WavWriter};
use songbird::input::core::{meta::StandardVisualKey::Media, sample};
use symphonia::core::{
    audio::{AudioSpec, GenericAudioBufferRef},
    codecs::audio::{AudioDecoder, AudioDecoderOptions},
    errors::Error,
    formats::{FormatOptions, FormatReader, Track, TrackType, probe::Hint},
    io::MediaSourceStream,
    meta::MetadataOptions,
    packet::Packet,
};

#[derive(Default, Debug)]
struct AudioBuffer {
    audio_stream: Vec<f32>,
    sample_rate: u32,
    channel_count: u16,
}

impl AudioBuffer {
    fn is_empty(&self) -> bool {
        self.audio_stream.is_empty()
    }

    fn reset(&mut self) {
        self.audio_stream.clear();
        // other vars don't matter
    }

    fn append_metadata(&mut self, sample_rate: u32, channel_count: u16) {
        assert_ne!(sample_rate, 0);
        assert_ne!(channel_count, 0);

        if self.audio_stream.is_empty() {
            self.sample_rate = sample_rate;
            self.channel_count = channel_count;
        } else {
            if sample_rate != self.sample_rate {
                // TODO: adapt
                panic!("sample rate is not constant")
            }
            if channel_count != self.channel_count {
                // TODO: adapt
                panic!("number of channels is not constant")
            }
        }
    }

    fn append_packet(&mut self, sample_rate: u32, decoded: GenericAudioBufferRef<'_>) {
        // FIXME: fail gracefully (skip packet?)
        let channel_count: u16 = decoded.spec().channels().count().try_into().unwrap();

        self.append_metadata(sample_rate, channel_count);

        let total_samples = self.audio_stream.len();
        self.audio_stream
            .resize(total_samples + decoded.samples_interleaved(), f32::MIN);

        decoded.copy_to_slice_interleaved(&mut self.audio_stream[total_samples..]);
    }

    fn append(&mut self, other: &AudioBuffer) {
        assert!(
            !other.is_empty(),
            "attempted to append an empty audio buffer"
        );
        self.append_metadata(other.sample_rate, other.channel_count);
        self.audio_stream.extend_from_slice(&other.audio_stream);
    }
}

#[tokio::main]
async fn main() {
    let url = String::from("https://public.share.bluegaria.net/GUT_GENUG.mp3");

    modify_audio(url).await;

    // let nums = bytes.into_iter().flatten().collect::<Vec<_>>();

    // let cursor = Cursor::new(nums);

    // let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

    // let mut format = symphonia::default::get_probe()
    //     .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
    //     .expect("Unsupported format");

    // let tracks = format.tracks();
    // for track in tracks {
    //     println!("{:#?}", track.duration);
    // }

    // let track = format.default_track(TrackType::Audio).expect("No audio track");

    // let mut decoder = symphonia::default::get_codecs().make_audio_decoder(
    //     track.codec_params.as_ref().expect("Codec parameters missing").audio().unwrap(),
    //     &AudioDecoderOptions::default())
    //     .expect("Unsupported codec");

    // let sample_rate = decoder.codec_params().sample_rate.unwrap();
    // let track_id = track.id;

    // let mut samples: Vec<f32> = Default::default();
    // println!("{:#?}", sample_rate);
    // let mut total_sample_count = 0;
    // let mut audio_spec: u16 = 0;

    // loop {
    //     let packet = match format.next_packet() {
    //         Ok(Some(packet)) => packet,
    //         Ok(None) => {
    //             break;
    //         }
    //         Err(Error::ResetRequired) => {
    //             break;
    //         }
    //         Err(err) => {
    //             panic!("{}", err);
    //         }
    //     };

    //     while !format.metadata().is_latest() {
    //         format.metadata().pop();
    //     }

    //     if packet.track_id != track_id {
    //         continue;
    //     }

    //     match decoder.decode(&packet) {
    //         Ok(_decoded) => {
    //             audio_spec = _decoded.spec().channels().count().try_into().unwrap();

    //             samples.resize(total_sample_count + _decoded.samples_interleaved(), f32::MIN);

    //             _decoded.copy_to_slice_interleaved(&mut samples[total_sample_count..]);

    //             total_sample_count += _decoded.samples_interleaved();
    //         }
    //         Err(Error::IoError(_)) => {
    //             continue;
    //         }
    //         Err(Error::DecodeError(_)) => {
    //             continue;
    //         }
    //         Err(err) => {
    //             panic!("{}", err);
    //         }
    //     }
    // }

    // let mut reverb = reverb_stereo(15.0, 3.0, 0.5);

    // reverb.set_sample_rate(sample_rate.into());
    // reverb.reset();

    // for frame in samples.chunks_exact_mut(2) {
    //     let (l, r) = reverb.filter_stereo(frame[0], frame[1]);

    //     frame[0] = l;
    //     frame[1] = r;
    // }

    // // for sample in samples {
    // //     println!("{:#?}", sample);
    // // }

    // let channel_num: u16 = audio_spec;

    // let spec = WavSpec {
    //     channels: channel_num,
    //     sample_rate: sample_rate,
    //     bits_per_sample: 32,
    //     sample_format: hound::SampleFormat::Float
    // };

    // let mut writer = WavWriter::create("output.wav", spec).unwrap();

    // for sample in samples {
    //     writer.write_sample(sample).expect("Error in write sample");
    // }

    // writer.finalize().expect("Error in finalize");
}

async fn modify_audio(url: String) {
    let mut audio = AudioBuffer::default();

    let result = reqwest::get(url).await;

    if result.is_err() {
        println!("Error");
        return;
    }

    let stream = result.unwrap().bytes_stream();

    let (download, source) =
        web_media_source::stream_media(stream.map_err(|e| io::Error::other(e)));

    let (a_r, b_r) = futures_util::future::join(
        download,
        tokio::task::spawn_blocking(move || {
            let mss = MediaSourceStream::new(Box::new(source), Default::default());

            let mut format = symphonia::default::get_probe()
                .probe(
                    &Hint::new(),
                    mss,
                    FormatOptions::default(),
                    MetadataOptions::default(),
                )
                .expect("Unsupported format");

            let track = format
                .default_track(TrackType::Audio)
                .expect("No audio track")
                .clone();

            let mut decoder = symphonia::default::get_codecs()
                .make_audio_decoder(
                    track
                        .codec_params
                        .as_ref()
                        .expect("Codec parameters missing")
                        .audio()
                        .unwrap(),
                    &AudioDecoderOptions::default(),
                )
                .expect("Unsupported codec");

            let mut reverb = reverb_stereo(15.0, 3.0, 0.5);

            reverb.set_sample_rate(decoder.codec_params().sample_rate.unwrap() as f64);
            reverb.reset();

            // don't want to collect this into one big buffer in prod but we're just testing
            let mut packet_audio = AudioBuffer::default();

            for packet_i in 0u64.. {
                let packet = match format.next_packet() {
                    Ok(Some(packet)) => packet,
                    Ok(None) => {
                        break;
                    }
                    Err(Error::ResetRequired) => {
                        break;
                    }
                    Err(err) => {
                        panic!("{}", err);
                    }
                };

                while !format.metadata().is_latest() {
                    format.metadata().pop();
                }

                println!("Packet {}: {} bytes", packet_i, packet.data.len());

                let track_id = track.id;
                if packet.track_id != track_id {
                    println!("Packet {} [skipped ]", packet_i);
                    continue;
                }

                // FIXME: fail gracefully (abort?), also do this once instead of every packet
                let sample_rate = decoder.codec_params().sample_rate.unwrap();

                let decoded = match decoder.decode(&packet) {
                    Ok(t) => t,
                    Err(err @ Error::DecodeError(_)) => {
                        // FIXME: fail gracefully (skip?)
                        return Err(io::Error::other(err));
                    }
                    Err(err) => return Err(io::Error::other(err)),
                };

                packet_audio.reset();
                packet_audio.append_packet(sample_rate, decoded);

                println!("Packet {} [decoded ]", packet_i);
                // println!("{:#?}", packet_audio.audio_stream);

                reverb_mod(&mut packet_audio, &mut reverb);

                println!("Packet {} [reverbed]", packet_i);
                // println!("{:#?}", packet_audio.audio_stream);

                audio.append(&packet_audio);
            }

            write_to_file(audio);

            io::Result::Ok(())
        }),
    )
    .await;

    a_r.expect("the downloader errored");
    b_r.expect("the decoder thread panicked")
        .expect("the decoder thread errored");
}

// fn decode_bytes(bytes: &Vec<u8>) -> Audio {
//     let cursor = Cursor::new(bytes);

//     let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

//     let mut format = symphonia::default::get_probe()
//         .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
//         .expect("Unsupported format");

//     let tracks = format.tracks();

//     let track = format.default_track(TrackType::Audio).expect("No audio track");

//     let mut decoder = symphonia::default::get_codecs().make_audio_decoder(
//         track.codec_params.as_ref().expect("Codec parameters missing").audio().unwrap(),
//         &AudioDecoderOptions::default())
//         .expect("Unsupported codec");

//     let sample_rate = decoder.codec_params().sample_rate.unwrap();

//     let track_id = track.id;

//     let mut samples: Vec<f32> = Default::default();
//     let mut total_sample_count = 0;
//     let mut audio_spec: u16 = 0;

//     loop {
//         let packet = match format.next_packet() {
//             Ok(Some(packet)) => packet,
//             Ok(None) => {
//                 break;
//             }
//             Err(Error::ResetRequired) => {
//                 break;
//             }
//             Err(err) => {
//                 panic!("{}", err);
//             }
//         };

//         while !format.metadata().is_latest() {
//             format.metadata().pop();
//         }

//         if packet.track_id != track_id {
//             continue;
//         }

//         match decoder.decode(&packet) {
//             Ok(_decoded) => {
//                 audio_spec = _decoded.spec().channels().count().try_into().unwrap();

//                 samples.resize(total_sample_count + _decoded.samples_interleaved(), f32::MIN);

//                 _decoded.copy_to_slice_interleaved(&mut samples[total_sample_count..]);

//                 total_sample_count += _decoded.samples_interleaved();
//             }
//             Err(Error::IoError(_)) => {
//                 continue;
//             }
//             Err(Error::DecodeError(_)) => {
//                 continue;
//             }
//             Err(err) => {
//                 panic!("{}", err);
//             }
//         }
//     }

//     let decoded_audio = Audio{audio_stream: samples, sample_rate: sample_rate, channel_num: audio_spec};

//     return decoded_audio;
// }

fn reverb_mod(decoded_audio: &mut AudioBuffer, reverb: &mut impl AudioUnit) {
    assert_eq!(decoded_audio.channel_count, 2);
    for frame in decoded_audio.audio_stream.chunks_exact_mut(2) {
        let (l, r) = reverb.filter_stereo(frame[0], frame[1]);

        frame[0] = l;
        frame[1] = r;
    }
}

fn write_to_file(audio: AudioBuffer) {
    let spec = WavSpec {
        channels: audio.channel_count,
        sample_rate: audio.sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };

    if audio.channel_count == 0 || audio.sample_rate == 0 {
        panic!("No decoded audio was produced");
    }

    let mut writer = WavWriter::create("output.wav", spec).unwrap();

    for sample in audio.audio_stream {
        writer.write_sample(sample).expect("Error in write sample");
    }

    writer.finalize().expect("Error in finalize");
}
