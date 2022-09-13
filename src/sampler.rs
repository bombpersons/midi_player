use std::io;
use std::path::Path;
use std::fs::File;

use wav::{Header, BitDepth};

struct WavData {
    header: Header,
    samples: BitDepth
}

fn read_wav_file(filepath: &Path) -> io::Result<WavData> {
    // Open the file...
    let mut file = File::open(filepath)?;
    let (header, data) = wav::read(&mut file)?;

    Ok(WavData {
        header,
        samples: data,
    })
}

struct Sample {
    data: WavData,
    midi_note: u8
}

pub struct Sampler {
    // The wav data for the samples to use.
    // Multiple can be used, the nearest one 
    // to the desired note will be used.
    samples: Vec<Sample>,
}

impl Sampler {
    // Create a sampler from a single file.
    pub fn from_single_file(filepath: &Path, midi_note: u8) -> Self {
        let wav = read_wav_file(filepath)
            .expect(format!("Couldn't load wave file at {}", filepath.display()).as_str());

        let mut samples = Vec::new();
        samples.push(Sample { data: wav, midi_note });

        Self {
            samples,
        }
    }

    // Retrieve the samples for a particular note. Returns the number of samples returned.
    // TODO: This only works with f32 wav data. + it doesn't do anything about sample rate.
    pub fn get_samples(&mut self, midi_note: u8, progress: usize, output_channels: usize, output: &mut [f32]) -> usize {
        // TODO: support multiple different samples.
        if self.samples.len() != 1 {
            return 0;
        }

        // TODO: Assuming f32 samples.
        let sample = self.samples.get(0).unwrap();
        let samples = sample.data.samples.as_thirty_two_float().unwrap();
        let sample_channels = sample.data.header.channel_count as usize;
        let sample_length = samples.len() / sample_channels;

        // If we are past the end of the sample
        if progress >= samples.len() {
            return 0;
        }


        let mut sampled = 0;
        for frame in output.chunks_exact_mut(output_channels) {
            let sample_count = progress + sampled;

            // Not past the end of the sample?
            if sample_count >= sample_length {
                break;
            }

            // Get the data from each channel.
            // If there are more channels in the sample than in the output,
            // the ignore some of the sample channels.
            // If there are less in the sample then duplicate them.
            for (output_channel, o) in frame.iter_mut().enumerate() {
                let channel_to_sample = (sample_channels-1).min(output_channel);
                *o = *samples.get((progress + sampled) * sample_channels + channel_to_sample).unwrap();
            }
            sampled += 1;
        }

        sampled
    }
}