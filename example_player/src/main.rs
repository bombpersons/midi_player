use std::thread;
use std::{path::Path, time::Duration, sync::Arc};
use std::io::{Write, Read};

use rusty_sample_player::midi::MidiPlayer;
use rusty_sample_player::sampler::{Sampler, SamplerBank, SamplerSynth};
use cpal::{traits::{HostTrait, DeviceTrait, StreamTrait}, Sample};
use tracing_subscriber::FmtSubscriber;

fn main() {
    // For tracing.
    // a builder for `FmtSubscriber`.
    let subscriber = FmtSubscriber::builder()
        // all spans/events with a level higher than TRACE (e.g, debug, info, warn, etc.)
        // will be written to stdout.
        .with_max_level(tracing::Level::DEBUG)
        // completes the builder.
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("setting default subscriber failed.");

    tracing::info!("Starting player...");

    // Default audio host.
    let host = cpal::default_host();

    // default output device.
    let device = host.default_output_device().expect("no output device available");

    // supported output streams
    let mut supported_configs_range = device.supported_output_configs()
        .expect("error querying configs");

    // Get best quality one.
    let supported_config = supported_configs_range.next()
        .expect("no configs!")
        .with_max_sample_rate().config();

    let mut test_bank = SamplerBank::from_json_file(Path::new("test_samples/sampler_bank_tod.json")).unwrap();
    test_bank.load_samplers().unwrap();
    //test_bank.resample(supported_config.sample_rate.0 as u16);

    // load a test midi file.
    let (sampler_synth, mut sampler_synth_output) = 
        SamplerSynth::new(test_bank, supported_config.sample_rate.0 as usize, supported_config.channels as usize);

    let mut midi_player = MidiPlayer::new(sampler_synth).expect("Couldn't create new midi player.");
    midi_player.load_from_file(Path::new("test_mid/tod.mid"));
    midi_player.play();

    // build the stream
    let stream = device.build_output_stream(
        &supported_config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let channel_count = supported_config.channels;

            let mut written = 0;
            while written < data.len() {
                written += sampler_synth_output.get_samples(channel_count as u8, &mut data[written..]);
            }
        },
        move |err| {
            tracing::warn!("error! {}", err);
        }
    ).unwrap();
    stream.play().unwrap();

    loop {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)
            .expect("Error getting command.");

        let args: Vec<&str> = line.trim().split(' ').collect();
        if args.len() > 0 {
            let command = args[0];

            tracing::info!("Command: {}", command);

            match command {
                "stop" => midi_player.stop(),
                "play" => midi_player.play(),
                "pause" => midi_player.pause(),
                "load" => {
                    if args.len() >= 2 {
                        midi_player.load_from_file(Path::new(args[1]))
                    } else {
                        tracing::info!("No midi file specified to load.")
                    }
                },
                "exit" => break,
                _ => tracing::info!("Invalid command '{}'", command)
            }
        }
    }
}
