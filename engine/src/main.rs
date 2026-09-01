mod web_media_source;

use std::fs;
use std::io;
use std::println;
use std::collections::HashMap;

use fundsp::typenum::Mod;
use fundsp::{
    prelude::{AudioNode, AudioUnit, bell_hz, db_amp},
    prelude32::{reverb_stereo, chorus, lowpass_hz, pass},
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

#[derive(Debug, Eq, Hash, PartialEq)]
enum Modification {
    Unmodified,
    Reverb,
    Chorus,
    Lowpass,
    Volume
}

impl TryFrom<&str> for Modification {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "unmodified" => Ok(Modification::Unmodified),
            "reverb" => Ok(Modification::Reverb),
            "chorus" => Ok(Modification::Chorus),
            "lowpass" => Ok(Modification::Lowpass),
            "volume" => Ok(Modification::Volume),
            _ => Err("Invalid string for Modifications"),
        }
    }
}

#[tokio::main]
async fn main() {
    run_program().await;
}

async fn run_program() {
    let url = String::from("https://public.share.bluegaria.net/GUT_GENUG.mp3");

    println!("Welcome to Entroximer!");
    println!("Build your digital signal processing graph by entering the numbers associated with the modifications, separated by '=>'.");
    println!("Options: unmodified, reverb, chorus, lowpass, volume");

    let mut input = String::new();

    io::stdin()
        .read_line(&mut input)
        .expect("Failed to read user input");


    let user_mod_list_result = validate_input(&mut input);

    if user_mod_list_result.is_err() {
        println!("Error: {:#?}", user_mod_list_result.err());
        return;
    }

    let user_mod_list = user_mod_list_result.unwrap();

    modify_audio(url, user_mod_list).await;
}

fn validate_input(input: &mut String) -> Result<HashMap<Modification, Vec<f32>>, String> {

    let mut mods: HashMap<Modification, Vec<f32>> = HashMap::new();

    let mut lower = input.to_lowercase();

    lower.retain(|c| !c.is_whitespace());

    let split_input: Vec<&str> = lower.split("=>").collect();

    if split_input.len() < 1 {
        return Err(String::from("User input is empty"));
    }

    for mod_and_param in split_input.iter() {

        let split_mod_and_param: Vec<&str> = mod_and_param.split('(').collect();

        match Modification::try_from(split_mod_and_param[0]) {
            Ok(modification) => {
                let mut parameters: Vec<f32> = Vec::new();
                let mut str_parameters: Vec<&str> = split_mod_and_param[1].split([',', ')']).collect();
                str_parameters.retain(|c| *c != "");

                for num in str_parameters.iter() {
                    println!("num: {:#?}", num);
                    let num_parsed: f32 = num.parse().expect("Non-numeric parameter");
                    parameters.push(num_parsed);
                }

                mods.insert(modification, parameters);
            }
            Err(e) => {
                return Err(e.to_string());
            }
        }
    }

    return Ok(mods);
}

fn convert_modification_add_to_list(modification: &Modification, parameters: &Vec<f32>, mod_list: &mut Vec<Box<dyn AudioUnit>>, sample_rate: f64) {
    if parameters.len() != num_parametres(modification) {
        panic!("Invalid number of paramters.");
    }

    match modification {
        Modification::Unmodified => { }
        Modification::Reverb => { 
            let mut reverb = reverb_stereo(parameters[0], parameters[1], parameters[2]);

            reverb.set_sample_rate(sample_rate);
            reverb.reset();

            mod_list.push(Box::from(reverb)) 
        }
        Modification::Chorus => {
            let chorus_left = chorus(0, parameters[0], parameters[1], parameters[2]);
            let chorus_right = chorus(1, parameters[0], parameters[1], parameters[2]);

            let mut chorus_stereo = chorus_left | chorus_right;
            
            chorus_stereo.set_sample_rate(sample_rate);
            chorus_stereo.reset();
            
            mod_list.push(Box::new(chorus_stereo));
        }
        Modification::Lowpass => {
            let lowpass_left = lowpass_hz(parameters[0], parameters[1]);
            let lowpass_right = lowpass_hz(parameters[0], parameters[1]);

            let mut lowpass_stereo = lowpass_left | lowpass_right;
            
            lowpass_stereo.set_sample_rate(sample_rate);
            lowpass_stereo.reset();
            
            mod_list.push(Box::new(lowpass_stereo));
        },
        Modification::Volume => {
            let mut volume_node = (pass() | pass()) * parameters[0];
            
            volume_node.set_sample_rate(sample_rate);
            volume_node.reset();
            
            mod_list.push(Box::new(volume_node));
        }
        _ => { panic!("Unrecognized modification!") }
    }
}

async fn modify_audio(url: String, modification_list: HashMap<Modification, Vec<f32>>) {
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

            let mut modification_units: Vec<Box<dyn AudioUnit>>= Vec::new();

            for (modification, parameters) in &modification_list {
                convert_modification_add_to_list(modification,  parameters, &mut modification_units, decoder.codec_params().sample_rate.unwrap() as f64);
            }

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

                apply_mods(&mut packet_audio, &mut modification_units);

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

fn apply_mods(decoded_audio: &mut AudioBuffer, mods: &mut Vec<Box<dyn AudioUnit>>) {
    if mods.len() < 1 {
        return;
    }

    assert_eq!(decoded_audio.channel_count, 2);
    for frame in decoded_audio.audio_stream.chunks_exact_mut(2) {
        for m in mods.iter_mut() {
            let (l, r) = m.filter_stereo(frame[0], frame[1]);

            frame[0] = l;
            frame[1] = r;
        }
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

fn num_parametres(modification: &Modification) -> usize {
    match modification {
        Modification::Unmodified => {
            return 0;
        }
        Modification::Reverb => {
            return 3;
        }
        Modification::Chorus => {
            return 3;
        }
        Modification::Lowpass => {
            return 2;
        }
        Modification::Volume => {
            return 1;
        }
        _ => {
            panic!("Unknown modification");
        }
    }
}