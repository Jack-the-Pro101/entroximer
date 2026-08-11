use std::io::Cursor;
use std::fs;

use futures_util::StreamExt;
use bytes::Bytes;
use hound::{WavSpec, WavWriter};
use songbird::input::core::{meta::StandardVisualKey::Media, sample};
use symphonia::core::{audio::{AudioSpec, GenericAudioBufferRef}, codecs::audio::{AudioDecoder, AudioDecoderOptions}, errors::Error, formats::{FormatOptions, FormatReader, Track, TrackType, probe::Hint}, io::MediaSourceStream, meta::MetadataOptions, packet::Packet};
use fundsp::{prelude::{AudioNode, AudioUnit, bell_hz, db_amp}, prelude32::reverb_stereo};

#[derive(Default, Debug)]
struct Audio {
    audio_stream: Vec<f32>,
    sample_rate: u32,
    channel_num: u16
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
    let mut x = 0;
    let mut audio = Audio::default();

    let result = reqwest::get(url).await;

    if result.is_err() {
        println!("Error");
        return;
    }

    let mut stream = result.unwrap().bytes_stream();

    let head = stream.next().await.expect("Problem").unwrap().to_vec();

    let cursor = Cursor::new(head);

    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

    let mut format = symphonia::default::get_probe()
        .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
        .expect("Unsupported format");

    let track = format.default_track(TrackType::Audio).expect("No audio track").clone();
    
    let mut decoder = symphonia::default::get_codecs().make_audio_decoder(
        track.codec_params.as_ref().expect("Codec parameters missing").audio().unwrap(),
        &AudioDecoderOptions::default())
        .expect("Unsupported codec");

    let mut reverb = reverb_stereo(15.0, 3.0, 0.5);

    reverb.set_sample_rate(decoder.codec_params().sample_rate.unwrap() as f64);
    reverb.reset();

    while let Some(item) = stream.next().await {
        if item.is_err() {
            println!("Error");
        }

        println!("Run {}", x);

        let bytes = &item.unwrap().to_vec();

        println!("Item: {:#?}", bytes.len());

        let mut decoded_audio = decode_bytes(bytes, &mut format, &track, &mut decoder);

        println!("Run {}: {:#?}", x, decoded_audio.audio_stream);


        reverb_mod(&mut decoded_audio, &mut reverb);

        if decoded_audio.channel_num != 0 {
            audio.sample_rate = decoded_audio.sample_rate;
            audio.channel_num = decoded_audio.channel_num;
        }

        // println!("Run {}: {:#?}", x, decoded_audio.audio_stream);

        audio.audio_stream.append(&mut decoded_audio.audio_stream);

        x += 1;
    }

    write_to_file(audio);
}

fn decode_bytes(bytes: &Vec<u8>, format: &mut Box<dyn FormatReader>, track: &Track, decoder: &mut Box<dyn AudioDecoder>) -> Audio {
    let sample_rate = decoder.codec_params().sample_rate.unwrap();

    let track_id = track.id;

    let mut samples: Vec<f32> = Default::default();
    let mut total_sample_count = 0;
    let mut audio_spec: u16 = 0;

    loop {
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

        if packet.track_id != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(_decoded) => {
                audio_spec = _decoded.spec().channels().count().try_into().unwrap();

                samples.resize(total_sample_count + _decoded.samples_interleaved(), f32::MIN);

                _decoded.copy_to_slice_interleaved(&mut samples[total_sample_count..]);

                total_sample_count += _decoded.samples_interleaved();
            }
            Err(Error::IoError(_)) => {
                continue;
            }
            Err(Error::DecodeError(_)) => {
                continue;
            }
            Err(err) => {
                panic!("{}", err);
            }
        }
    }

    let decoded_audio = Audio{audio_stream: samples, sample_rate: sample_rate, channel_num: audio_spec};

    return decoded_audio;
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

fn reverb_mod(decoded_audio: &mut Audio, reverb: &mut impl AudioUnit) {
    for frame in decoded_audio.audio_stream.chunks_exact_mut(2) {
        let (l, r) = reverb.filter_stereo(frame[0], frame[1]);

        frame[0] = l;
        frame[1] = r;
    }
}

fn write_to_file(audio: Audio) {
    let spec = WavSpec {
        channels: audio.channel_num,
        sample_rate: audio.sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float
    };

    if audio.channel_num == 0 || audio.sample_rate == 0 {
        panic!("No decoded audio was produced");
    }

    let mut writer = WavWriter::create("output.wav", spec).unwrap();

    for sample in audio.audio_stream {
        writer.write_sample(sample).expect("Error in write sample");
    }

    writer.finalize().expect("Error in finalize");
}