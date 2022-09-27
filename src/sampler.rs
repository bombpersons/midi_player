use std::collections::{HashSet, HashMap};
use std::io::{Read, Seek};
use std::mem::take;
use std::process::Output;
use std::sync::mpsc::{Receiver, channel, Sender};
use std::thread::{JoinHandle, self};
use std::time::{Instant, Duration};
use std::{io, fs};
use std::path::{Path, PathBuf};
use std::fs::File;
use std::hash::Hash;

use itertools::Itertools;
use midly::MidiMessage;
use ringbuf::{RingBuffer, Consumer};
use rubato::{SincFixedIn, InterpolationParameters, InterpolationType, ResamplerConstructionError, Resampler, ResampleError};
use serde::{Deserialize, Serialize};

use timer::{Timer, Guard};
use wav::{Header, BitDepth};

use crate::midi::{self, Synth, Frame};

struct Channel {
    samples: Vec<f32>
}

#[derive(Debug)]
pub enum WavDataError {
    IoError(io::Error),
    ResamplerConstructionError(ResamplerConstructionError),
    ResampleError(ResampleError)
}

impl From<io::Error> for WavDataError {
    fn from(e: io::Error) -> Self {
        WavDataError::IoError(e)
    }
}

impl From<ResamplerConstructionError> for WavDataError {
    fn from(e: ResamplerConstructionError) -> Self {
        WavDataError::ResamplerConstructionError(e)
    }
}

impl From<ResampleError> for WavDataError {
    fn from(e: ResampleError) -> Self {
        WavDataError::ResampleError(e)
    }
}

struct WavData {
    sample_rate: u16,
    channels: Vec<Channel>,
    sample_length: usize
}

impl WavData {
    pub fn from_file(filepath: &Path) -> Result<Self, WavDataError> {
        tracing::info!("Loading {}", filepath.display());

        // Open the file...
        let mut file = File::open(filepath)?;
        Self::from_reader(&mut file)
    }

    pub fn from_reader<T: Read + Seek>(reader: &mut T) -> Result<Self, WavDataError> {
        let (header, data) = wav::read(reader)?;

        // Convert samples to f32
        let samples = match data {
            BitDepth::ThirtyTwoFloat(samples) => samples,
            BitDepth::TwentyFour(samples) => samples
                .iter().map(|s| ((*s as f32 + i32::MAX as f32) / (i32::MAX as f32 - i32::MIN as f32) - 0.5) * 2.0).collect(),
            BitDepth::Sixteen(samples) => samples
                .iter().map(|s| ((*s as f32 + i16::MAX as f32) / (i16::MAX as f32 - i16::MIN as f32) - 0.5) * 2.0).collect(),
            BitDepth::Eight(samples) => samples
                .iter().map(|s| ((*s as f32 / u8::MAX as f32) - 0.5) * 2.0).collect(),
            BitDepth::Empty => Vec::new()
        };

        // Format the data. Separate the channels into vectors
        // for easy access. Plus our resampler needs them like this.
        let mut channels = Vec::new();
        for _ in 0..header.channel_count {
            channels.push(Channel {
                samples: Vec::new()
            });
        }
        for frame in samples.chunks_exact(header.channel_count as usize) {
            for (i, s) in frame.iter().enumerate() {
                channels[i].samples.push(*s);
            }
        }

        // Record the length of the sample. All channels *should* be the same length.
        let sample_length = if channels.len() > 0 {
            channels[0].samples.len()
        } else {
            0
        };

        let data = WavData {
            sample_rate: header.sampling_rate as u16,
            channels,
            sample_length
        };
        Ok(data)
    }

    pub fn resample(&self, new_sample_rate: u16) -> Result<Self, WavDataError> {
        tracing::info!("Resampling from {} samples per second to {} samples per second...", self.sample_rate, new_sample_rate);

        // Create the resampler...
        // I have no idea what these options really do, just using the ones used on the readme.
        let params = InterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: InterpolationType::Linear,
            oversampling_factor: 256,
            window: rubato::WindowFunction::BlackmanHarris2
        };
        let mut resampler = SincFixedIn::<f32>::new(
            new_sample_rate as f64 /  self.sample_rate as f64,
            2.0,
            params,
            self.sample_length,
            self.channels.len()
        )?;

        // Construct the vectors required for rubato
        let mut waves_in = Vec::new();
        for channel in self.channels.iter() {
            waves_in.push(channel.samples.to_vec());
        }

        // Process
        let waves_out = resampler.process(&waves_in, None)?;

        // Put the data back into channels
        let mut channels = Vec::new();
        for c in waves_out.iter() {
            channels.push(Channel { samples: c.to_vec() });
        };

        // Construct the new wavdata.
        let new = Self {
            channels,
            sample_length: self.sample_length,
            sample_rate: new_sample_rate
        };

        tracing::info!("Resampled.");

        Ok(new)
    }
}

#[derive(Debug)]
pub enum SampleError {
    SampleNotLoaded,
    ChannelOutOfBounds,
    SampleOutOfBounds,
    WavDataError(WavDataError)
}

impl From<WavDataError> for SampleError {
    fn from(e: WavDataError) -> Self {
        SampleError::WavDataError(e)
    }
}

#[derive(Serialize, Deserialize)]
struct Sample {
    filepath: PathBuf,
    midi_note: u8,

    #[serde(skip_serializing)]
    #[serde(skip_deserializing)]
    data: Option<WavData>
}

impl Sample {
    pub fn load(&mut self) -> Result<(), SampleError> {
        self.data = Some(WavData::from_file(&self.filepath)?);
        Ok(())
    }

    pub fn resample(&mut self, new_sample_rate: u16) -> Result<(), SampleError> {
        if self.data.is_none() {
            return Err(SampleError::SampleNotLoaded);
        }

        self.data = Some(self.data.as_ref().unwrap().resample(new_sample_rate)?);
        Ok(())
    }

    pub fn get_channel(&self, channel: usize) -> Result<&Channel, SampleError> {
        if self.data.is_none() {
            return Err(SampleError::SampleNotLoaded);
        }

        let channel = self.data.as_ref().unwrap().channels.get(channel);
        match channel {
            Some(c) => Ok(c),
            None => Err(SampleError::ChannelOutOfBounds)
        }
    }
    
    pub fn get_sample_length(&self) -> Result<usize, SampleError> {
        if self.data.is_none() {
            return Err(SampleError::SampleNotLoaded);
        }

        Ok(self.data.as_ref().unwrap().sample_length)
    }

    pub fn get_sample_rate(&self) -> Result<u16, SampleError> {
        if self.data.is_none() {
            return Err(SampleError::SampleNotLoaded);
        }
        Ok(self.data.as_ref().unwrap().sample_rate)
    }

    pub fn get_sample_channel_count(&self) -> Result<usize, SampleError> {
        if self.data.is_none() {
            return Err(SampleError::SampleNotLoaded);
        }
        Ok(self.data.as_ref().unwrap().channels.len())
    }

    pub fn get_sample_interpolated(&self, index: f32, channel: usize) -> Result<f32, SampleError> {
        let sample_channels = self.get_sample_channel_count()?;
        let channel = self.get_channel(channel)?;

        let low_sample_index = index.floor() as usize;
        let high_sample_index = index.ceil() as usize;

        // If we have no low sample error
        match channel.samples.get(low_sample_index) {
            None => Err(SampleError::SampleOutOfBounds),
            Some(low_sample) => {
                // Try and get the high sample and interpolate. If it doesn't exist, just return then low one.
                match channel.samples.get(high_sample_index) {
                    _ => Ok(*low_sample),
                    Some(high_sample) => {
                        // Interpolate between both samples.
                        let remainder = index - low_sample_index as f32;
                        Ok(low_sample + remainder * (high_sample - low_sample))
                    }
                }
            }
        }
    }

    pub fn get_sample(&self, index: usize, channel: usize) -> Result<f32, SampleError> {
        let sample_channels = self.get_sample_channel_count()?;
        let channel = self.get_channel(channel)?;

        match channel.samples.get(index) {
            Some(sample) => Ok(*sample),
            None => Err(SampleError::SampleOutOfBounds)
        }
    }

    pub fn get_samples(&self, progress: Duration, time_stopped: Option<Duration>, output_sample_rate: usize, desired_midi_note: u8, volume: f32, output_channels: usize, output: &mut [f32]) -> Result<usize, SampleError> {
        // Ratio of output samples per actual samples.
        let sample_rate = self.get_sample_rate()?;
        let sample_duration = 1.0 / sample_rate as f32;
        let output_sample_duration = Duration::from_secs_f32(1.0 / output_sample_rate as f32);
        let output_sample_num = output.len() / output_channels;

        // How many channels are in this sample.
        let sample_channels = self.get_sample_channel_count()?;

        // How much faster do we need to sample in order
        // to get the desired frequency?
        let desired_freq = midi::midi_note_to_freq(desired_midi_note);
        let sample_freq = midi::midi_note_to_freq(self.midi_note);
        let freq_ratio = desired_freq / sample_freq;

        // When do we stop sampling?
        let mut progress = progress;
        let progess_end = progress + output_sample_duration.mul_f32(output_sample_num as f32);

        let mut sampled = 0;
        while sampled < output_sample_num {
            // Calculate the sample index we need to be getting right now.
            let sample_index = progress.as_secs_f32() * sample_rate as f32 * freq_ratio;

            // Fill out each channel.
            for channel in 0..output_channels {
                let channel_to_sample = (sample_channels-1).min(channel);
                let sample = match self.get_sample_interpolated(sample_index, channel_to_sample) {
                    Err(SampleError::SampleOutOfBounds) => { // Sample out of bounds, return 0.0
                        Ok(0.0)
                    },
                    Ok(mut sample) => {
                        // Fade in the first moment of the sample to avoid clipping.
                        const fade_in_duration: f32 = 0.01;
                        let fade_in = (progress.as_secs_f32() / fade_in_duration).min(1.0);
                        sample *= fade_in;

                        // Fade out in the last moment of the sample to avoid clipping.
                        const fade_out_duration: f32 = 0.1;
                        if let Some(time_stopped) = time_stopped {
                            let duration_since_stopped = (progress - time_stopped).max(Duration::ZERO);
                            let fade_out = 1.0 - (duration_since_stopped.as_secs_f32() / fade_out_duration).min(1.0);

                            sample *= fade_out;
                        }

                        // Volume
                        sample *= volume;

                        Ok(sample)
                    },
                    Err(e) => Err(e), // Return the error unmodified
                }?;
                output[sampled*output_channels + channel] += sample;
            }

            sampled += 1;
            progress += output_sample_duration
        }

        Ok(sampled)
    }
}

#[derive(Debug)]
pub enum SamplerError {
    SampleError(SampleError),
    MismatchedSampleRates,
    NoSamplesFound,
}

impl From<SampleError> for SamplerError {
    fn from(e: SampleError) -> Self {
        SamplerError::SampleError(e)
    }
}

#[derive(Serialize, Deserialize)]
pub struct Sampler {
    name: String,
    // The wav data for the samples to use.
    // Multiple can be used, the nearest one 
    // to the desired note will be used.
    samples: Vec<Sample>,
}

impl Sampler {
    // Create a sampler from a single file.
    pub fn from_single_file(filepath: &Path, name: &str, midi_note: u8) -> Self {
        let wav = WavData::from_file(filepath)
            .expect(format!("Couldn't load wave file at {}", filepath.display()).as_str());

        let mut samples = Vec::new();
        samples.push(Sample { filepath: filepath.to_owned(), data: Some(wav), midi_note });

        Self {
            name: name.to_string(),
            samples,
        }
    }

    pub fn load_samples(&mut self) -> Result<(), SamplerError> {
        for sample in self.samples.iter_mut() {
            sample.load()?;
        }

        Ok(())
    }

    // Resample all samples to a new sample rate.
    pub fn resample(&mut self, new_sample_rate: u16) -> Result<(), SamplerError> {
        for sample in self.samples.iter_mut() {
            sample.resample(new_sample_rate)?;
        }
        Ok(())
    }

    // Retrieve the samples for a particular note. Returns the number of samples returned.
    pub fn get_samples(&mut self, output_sample_rate: usize, midi_note: u8, volume: f32, progress: Duration, time_stopped: Option<Duration>, output_channels: usize, output: &mut [f32]) -> Result<usize, SamplerError> {
        // Pick the sample with the closest midi note.
        let mut closest_sample = None;
        for sample in self.samples.iter() {
            closest_sample = match closest_sample {
                None => Some(sample),
                Some(closest) => {
                    if sample.midi_note.abs_diff(midi_note) < closest.midi_note.abs_diff(midi_note) {
                        Some(sample)
                    } else {
                        Some(closest)
                    }
                }
            }
        }
        if closest_sample.is_none() {
            return Err(SamplerError::NoSamplesFound);
        }

        let sample = closest_sample.unwrap();
        let sampled = sample.get_samples(progress, time_stopped, output_sample_rate, midi_note, volume, output_channels, output)?;
        Ok(sampled)
    }
}

#[derive(Debug)]
pub enum SamplerBankError {
    IoError(io::Error),
    MalformedJson(serde_json::Error),
    SamplerError(SamplerError)
}

impl From<io::Error> for SamplerBankError {
    fn from(e: io::Error) -> Self {
        Self::IoError(e)
    }
}

impl From<serde_json::Error> for SamplerBankError {
    fn from(e: serde_json::Error) -> Self {
        Self::MalformedJson(e)
    }
}

impl From<SamplerError> for SamplerBankError {
    fn from(e: SamplerError) -> Self {
        Self::SamplerError(e)
    }
}

#[derive(Serialize, Deserialize)]
pub struct SamplerBank {
    #[serde(skip_serializing)]
    #[serde(skip_deserializing)]
    samplers: Vec<Sampler>,

    voices: Vec<String>,
    folder: String
}

impl SamplerBank {
    pub fn from_json_file(filepath: &Path) -> Result<Self, SamplerBankError> {
        // Try and open the file.
        let mut file = File::open(filepath)?;
        Self::from_json_reader(file)
    }

    pub fn from_json_reader<T: Read>(reader: T) -> Result<Self, SamplerBankError> {
        // Try and parse it.
        let mut parsed: Self = serde_json::from_reader(reader)?;

        // Automatically find the samples in the folder.
        let paths = fs::read_dir(&parsed.folder)?;
        for sampler_dir_result in paths {
            let sampler_dir = sampler_dir_result?;
            if sampler_dir.metadata().unwrap().is_file() {
                continue;
            }

            let mut sampler = Sampler {
                name: sampler_dir.path().file_stem().unwrap().to_str().unwrap().to_string(),
                samples: Vec::new()
            };

            let samples = fs::read_dir(sampler_dir.path())?;
            for sample_file_result in samples {
                let sample_file = sample_file_result?;
                if let Some(ext) = sample_file.path().extension() {
                    if ext.to_str().unwrap() != "wav" {
                        continue;
                    }

                    let name = sample_file.path().file_stem().unwrap().to_str().unwrap().to_string();
                    let sample = Sample {
                        data: None,
                        filepath: sample_file.path(),
                        midi_note: midi::note_name_to_midi_note(name.as_str()).unwrap()
                    };
                    sampler.samples.push(sample);
                }
            }
            parsed.samplers.push(sampler);
        }

        Ok(parsed)
    }

    pub fn load_samplers(&mut self) -> Result<(), SamplerBankError> {
        for sampler in self.samplers.iter_mut() {
            sampler.load_samples()?;
        }
        Ok(())
    }

    pub fn resample(&mut self, new_sample_rate: u16) -> Result<(), SamplerBankError> {
        for sampler in self.samplers.iter_mut() {
            sampler.resample(new_sample_rate)?;
        }
        Ok(())
    }
}

pub struct SamplerSynth {
}

impl SamplerSynth {
    pub fn new() -> Self {
        Self {
        }
    }

    fn note_on(&mut self, key: usize, vel: usize) {
        tracing::debug!("Key {} on at {} velocity", key, vel);
    }

    fn note_off(&mut self, key: usize) {
        tracing::debug!("Key {} off", key);
    }
}

impl Synth for SamplerSynth {
    fn midi_message(&mut self, channel: usize, message: MidiMessage) {
        match message {
            MidiMessage::NoteOn { key, vel } => {
                if vel == 0 {
                    self.note_off(key.as_int() as usize);
                } else {
                    self.note_on(key.as_int() as usize, vel.as_int() as usize);
                }
            },
            MidiMessage::NoteOff { key, vel } => {
                self.note_off(key.as_int() as usize);
            },
            _ => ()
        }
    }

    fn gen_samples(&mut self, output_channel_count: usize, output: &mut [Frame]) -> usize {
        for frame in output.iter_mut() {
            match output_channel_count {
                1 => *frame = Frame::Mono(0.0),
                2 => *frame = Frame::Stereo(0.0, 0.0),
                _ => ()
            }
        }
        output.len()
    }
}