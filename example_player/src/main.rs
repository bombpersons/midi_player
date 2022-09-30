use std::{path::Path};

use rusty_sample_player::midi::{create_player, Frame};
use rusty_sample_player::sampler::{SamplerSynth, SamplerBank};

use cpal::{traits::{HostTrait, DeviceTrait, StreamTrait}};
use tracing_subscriber::FmtSubscriber;

fn main() {
    // For tracing.
    // a builder for `FmtSubscriber`.
    let subscriber = FmtSubscriber::builder()
        // all spans/events with a level higher than TRACE (e.g, debug, info, warn, etc.)
        // will be written to stdout.
        .with_max_level(tracing::Level::INFO)
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

    // Load the samples
    let mut test_bank = SamplerBank::from_json_file(Path::new("test_samples/sampler_bank.json")).unwrap();
    test_bank.load_samplers().unwrap();
    //test_bank.resample(supported_config.sample_rate.0 as u16);

    // Create the player.
    let synth = SamplerSynth::new(test_bank);
    let (_player_thread, mut player_controller, mut player_output)
         = create_player(supported_config.sample_rate.0 as usize, supported_config.channels as usize, synth);

    // let mut player_controller;
    // let mut player_output;
    // {
    //     let (player_thread, mut test_controller, mut test_output)
    //         = create_player(supported_config.sample_rate.0 as usize, supported_config.channels as usize, synth);
    //     player_controller = test_controller;
    //     player_output = test_output;
    // }

    player_controller.load_from_file(Path::new("test_mid/ff7.mid")).unwrap();
    player_controller.play().unwrap();

    // build the stream
    let stream = device.build_output_stream(
        &supported_config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let channel_count = supported_config.channels as usize;

            for output_frame in data.chunks_exact_mut(channel_count) {
                // Try to get a frame, if there is none show a warning that the samples are being
                // generated too slowly.
                let frame = player_output.get_next_frame();

                //tracing::info!("CPal frame: {:?}", frame);

                // Depending on mono or stereo write the samples to the audio device.
                match frame {
                    Frame::Mono(sample) => {
                        for channel in 0..channel_count {
                            output_frame[channel] = sample;
                        }
                    },
                    Frame::Stereo(left, right) => {
                        for channel in 0..channel_count {
                            output_frame[channel] = match channel {
                                0 => left,
                                1 => right,
                                _ => right // Duplicate for any more channels?
                            }
                        }
                    }
                }
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
                "stop" => player_controller.stop().unwrap(),
                "play" => player_controller.play().unwrap(),
                "pause" => player_controller.pause().unwrap(),
                "loop" => player_controller.toggle_loop().unwrap(),
                "load" => {
                    if args.len() >= 2 {
                        player_controller.load_from_file(Path::new(args[1])).unwrap()
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
