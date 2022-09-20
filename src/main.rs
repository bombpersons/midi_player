mod midi;
mod sampler;

use std::thread;
use std::{path::Path, time::Duration, sync::Arc};
use std::io::Write;

use chrono::Local;
use env_logger::Builder;
use log::LevelFilter;
use midi::MidiPlayer;
use sampler::{Sampler, SamplerBank, SamplerSynth};
use cpal::{traits::{HostTrait, DeviceTrait, StreamTrait}, Sample};
use tracing_subscriber::FmtSubscriber;

fn main() {
    // For logging.
    // Builder::new()
    //     .format(|buf, record| {
    //         writeln!(
    //             buf,
    //             "{} {}: {}",
    //             record.level(),
    //             //Format like you want to: <-----------------
    //             Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
    //             record.args()
    //         )
    //     })
    //     .filter(None, LevelFilter::Info)
    //     .init();

    // For tracing.
    // a builder for `FmtSubscriber`.
    let subscriber = FmtSubscriber::builder()
        // all spans/events with a level higher than TRACE (e.g, debug, info, warn, etc.)
        // will be written to stdout.
        .with_max_level(tracing::Level::WARN)
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

    let mut test_bank = SamplerBank::from_json_file(Path::new("sampler_bank.json")).unwrap();
    test_bank.load_samplers().unwrap();
    test_bank.resample(supported_config.sample_rate.0 as u16);

    // load a test midi file.
    let (sampler_synth, mut sampler_synth_output) = 
        SamplerSynth::new(test_bank, supported_config.sample_rate.0 as usize, supported_config.channels as usize);

    let mut midi_player = MidiPlayer::new(sampler_synth).expect("Couldn't create new midi player.");
    midi_player.load_from_file(Path::new("kingdom.mid"));
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
            log::info!("error! {}", err);
        }
    ).unwrap();
    stream.play().unwrap();

    loop {
        thread::sleep(Duration::from_secs(1));
    }
}
