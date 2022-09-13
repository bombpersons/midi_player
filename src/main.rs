mod midi;
mod sampler;

use std::path::Path;

use sampler::Sampler;
use cpal::{traits::{HostTrait, DeviceTrait, StreamTrait}, Sample};

fn main() {
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

    // a test sample
    let mut sampler = Sampler::from_single_file(Path::new("test_stereo.wav"), 10);
    let mut progress = 0;

    // build the stream
    let stream = device.build_output_stream(
        &supported_config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            // read / write to the stream here.
            // for sample in data.iter_mut() {
            //     *sample = Sample::from(&0.0);
            // }

            let channel_count = supported_config.channels;
            progress += sampler.get_samples(10, progress, channel_count as usize, data);
        },
        move |err| {
            println!("error! {}", err);
        }
    ).unwrap();
    stream.play().unwrap();

    loop {
    }
}
