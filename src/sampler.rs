use std::collections::{HashSet, HashMap};
use std::io::{Read, Seek};
use std::mem::take;
use std::sync::mpsc::{Receiver, channel, Sender};
use std::thread::{JoinHandle, self};
use std::time::{Instant, Duration};
use std::{io, fs};
use std::path::{Path, PathBuf};
use std::fs::File;
use std::hash::Hash;

use nodi::midly::num::{u7, u4};
use nodi::{Connection, Event, MidiEvent};
use nodi::midly::MidiMessage;
use ringbuf::{RingBuffer, Consumer};
use rubato::{SincFixedIn, InterpolationParameters, InterpolationType, ResamplerConstructionError, Resampler, ResampleError};
use serde::{Deserialize, Serialize};

use timer::{Timer, Guard};
use wav::{Header, BitDepth};

use crate::midi;

struct Channel {
    samples: Vec<f32>
}

#[derive(Debug)]
enum WavDataError {
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
        log::info!("Loading {}", filepath.display());

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
        log::info!("Resampling from {} samples per second to {} samples per second...", self.sample_rate, new_sample_rate);

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

        log::info!("Resampled.");

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

    pub fn get_sample(&self, index: usize, channel: usize) -> Result<f32, SampleError> {
        let sample_channels = self.get_sample_channel_count()?;
        let channel = self.get_channel(channel)?;

        match channel.samples.get(index) {
            Some(sample) => Ok(*sample),
            None => Err(SampleError::SampleOutOfBounds)
        }
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

    pub fn get_sample(&self, output_sample_rate: u16, midi_note: u8, progress: usize, channel: u8) -> Result<f32, SamplerError> {
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
        let sample_channels = sample.get_sample_channel_count()?;
        let sample_rate = sample.get_sample_rate()?;
        let sample_length = sample.get_sample_length()?;

        if output_sample_rate != sample_rate {
            log::info!("Sample rate of output doesn't match the input! Re-sample to match before playing.");
            return Err(SamplerError::MismatchedSampleRates);
        }

        // How much faster do we need to sample in order
        // to get the desired frequency?
        let desired_freq = midi::midi_note_to_freq(midi_note);
        let sample_freq = midi::midi_note_to_freq(sample.midi_note);
        let freq_ratio = desired_freq / sample_freq;

        // Where do we sample from...
        // If the desired frequency isn't the same as the natural frequency of the
        // sample, then we need to sample at a different rate.
        
        // Ignore some of the channels if there are less in the output.
        // Duplicate the sample channels if there are more in the output.
        let channel_to_sample = (sample_channels-1).min(channel as usize);

        let sample_index = (progress as f32  * freq_ratio) as usize;
        let output = sample.get_sample(sample_index, channel_to_sample)?;

        Ok(output)
    }

    // Retrieve the samples for a particular note. Returns the number of samples returned.
    pub fn get_samples(&mut self, output_sample_rate: u16, midi_note: u8, progress: usize, output_channels: u8, output: &mut [f32]) -> Result<usize, SamplerError> {
        // // TODO: support multiple different samples.
        // if self.samples.len() != 1 {
        //     return Err(SamplerError::MultipleSamplesNotSupported);
        // }

        // TODO: Assuming f32 samples.
        let sample = self.samples.get(0).unwrap();
        let sample_channels = sample.get_sample_channel_count()?;
        let sample_rate = sample.get_sample_rate()?;
        let sample_length = sample.get_sample_length()?;

        if output_sample_rate != sample_rate {
            log::info!("Sample rate of output doesn't match the input! Re-sample to match before playing.");
            return Err(SamplerError::MismatchedSampleRates);
        }

        // How much faster do we need to sample in order
        // to get the desired frequency?
        let desired_freq = midi::midi_note_to_freq(midi_note);
        let sample_freq = midi::midi_note_to_freq(sample.midi_note);
        let freq_ratio = desired_freq / sample_freq;

        let mut sampled = 0;
        for frame in output.chunks_exact_mut(output_channels as usize) {
            // Where do we sample from...
            // If the desired frequency isn't the same as the natural frequency of the
            // sample, then we need to sample at a different rate.
            let sample_index = ((progress + sampled) as f32  * freq_ratio) as usize;

            // Not past the end of the sample?
            if sample_index >= sample_length {
                break;
            }

            // Get the data from each channel.
            // If there are more channels in the sample than in the output,
            // the ignore some of the sample channels.
            // If there are less in the sample then duplicate them.
            for (output_channel, o) in frame.iter_mut().enumerate() {
                let channel_to_sample = (sample_channels-1).min(output_channel);
                *o = sample.get_sample(sample_index, channel_to_sample)?;
            }
            sampled += 1;
        }

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

pub struct SamplerSynthOutput {
    channel_count: usize,
    pub consumer: ringbuf::Consumer<f32>
}

impl SamplerSynthOutput {
    pub fn new(channel_count: usize, consumer: ringbuf::Consumer<f32>) -> Self {
        Self {
            channel_count,
            consumer
        }
    }

    pub fn get_samples(&mut self, output_channels: u8, output: &mut [f32]) -> usize {
        let mut sampled = 0;
        for frame in output.chunks_exact_mut(output_channels as usize) {
            // If the synth hasn't produced at least a single frame of audio, then early quit with no output.
            if self.consumer.len() < self.channel_count {
                log::info!("Not enough samples in buffer!");
                return sampled;
            }

            // *Should* be safe to just pop off a frame worth of samples.
            let mut synth_frame = Vec::new();
            for i in 0..self.channel_count {
                synth_frame.push(self.consumer.pop().unwrap())
            }

            //log::info!("{:?}", synth_frame);

            // Get the data from each channel.
            // If there are more channels in the sample than in the output,
            // the ignore some of the sample channels.
            // If there are less in the sample then duplicate them.
            for (output_channel, o) in frame.iter_mut().enumerate() {
                let channel_to_sample = (self.channel_count-1).min(output_channel);
                *o = synth_frame[channel_to_sample];
            }
            sampled += output_channels as usize;
        } 
        sampled
    }
}

pub struct Note {
    velocity: u8, 
    samples_played: usize
}

pub struct Tracks {
    tracks: HashMap<u4, HashMap<u7, Note>>
}

impl Tracks {
    pub fn new() -> Self {
        Self {
            tracks: HashMap::new()
        }
    }

    pub fn note_on(&mut self, key: u7, vel: u7, channel: u4) {
        //log::info!("Adding {} to channel {}...", key, channel);

        // Create a new note and add it to the appropriate track.
        let note = Note {
            velocity: vel.as_int(),
            samples_played: 0
        };

        // If the track doesn't exist add it.
        let track = self.tracks.entry(channel).or_insert_with(HashMap::new);
        track.insert(key, note);
    }

    pub fn note_off(&mut self, key: u7, channel: u4) {
        //log::info!("Removing {} from channel {}...", key, channel);

        // Find the note and remove it.
        self.tracks.get_mut(&channel)
            .map(|track| track.remove(&key));
    }

    pub fn for_each_note<F: FnMut(&u7, &mut Note, &u4)>(&mut self, func: &mut F) {
        for (channel, track) in self.tracks.iter_mut() {
            for (midi_note, note) in track.iter_mut() {
                func(midi_note, note, channel);
            }
        }
    }
}

pub struct SamplerSynth {
    message_sender: Sender<MidiEvent>,
    synth_thread: JoinHandle<()>
}

const SAMPLER_SYNTH_BUFFER_LENGTH: f32 = 0.1;

impl SamplerSynth {
    pub fn new(sampler_bank: SamplerBank, sample_rate: usize, channel_count: usize) -> (Self, SamplerSynthOutput) {
        // Ring buffer to store generated samples.
        let ring_buf = RingBuffer::new((sample_rate as f32 * SAMPLER_SYNTH_BUFFER_LENGTH).floor() as usize);
        let (producer, consumer) = ring_buf.split();

        // A thread for the synth to generate samples in.
        let (message_sender, message_receiver) = channel();
        let synth_thread = Self::create_synth_thread(
            sampler_bank, sample_rate, channel_count, message_receiver, producer);

        let synth = Self {
            message_sender,
            synth_thread
        };
        let output = SamplerSynthOutput::new(channel_count, consumer);
        (synth, output)
    }

    fn create_synth_thread(mut sampler_bank: SamplerBank, sample_rate: usize, channel_count: usize, receiver: Receiver<MidiEvent>, mut producer: ringbuf::Producer<f32>) -> JoinHandle<()> {
        let thread = thread::spawn(move || {
            // A list of tracks that contain a list of notes that are currently playing.
            let mut tracks = Tracks::new();

            // A timer so we know how many samples to produce.
            let mut samples_processed = 0;
            let mut time = Instant::now();
            let sample_duration = 1.0 / sample_rate as f32;

            let mut handle_event = |tracks: &mut Tracks, event: MidiEvent| {
                let message = event.message;
                match message {
                    MidiMessage::NoteOn { key, vel } => {
                        // The midi specs say that a NoteOn message with a velocity of 0
                        // should be treated the same as a NoteOff message.
                        if vel > 0 {
                            tracks.note_on(key, vel, event.channel);
                        } else {
                            tracks.note_off(key, event.channel);
                        }
                    },
                    MidiMessage::NoteOff { key, vel } => {
                        tracks.note_off(key, event.channel);
                    },
                    _ => ()
                }
            };

            loop { 
                // Get the next midi event.
                let received = receiver.recv_timeout(Duration::from_secs_f32(SAMPLER_SYNTH_BUFFER_LENGTH / 8.0));

                //thread::sleep(Duration::from_secs_f32(sample_duration * 100.0));
                let samples_needed = (time.elapsed().as_secs_f32() * sample_rate as f32).floor() as usize;
                let mut samples_to_produce = samples_needed - samples_processed;
                if samples_to_produce == 0 {
                    continue;
                }

                // Generate all the samples we need.
                let mut samples_produced = 0;
                samples_to_produce = (producer.remaining() / channel_count).min(samples_to_produce);

                for sample_count in 0..samples_to_produce {
                    // If the buffer is full we need to wait.
                    if producer.remaining() < channel_count {
                        log::info!("Buffer full!");
                        break;
                    }

                    // For each output channel.
                    for output_channel in 0..channel_count {
                        let mut sample = 0.0;
                        tracks.for_each_note(&mut |midi_note, note, channel| {
                            let voice_index = (channel.as_int() as usize).min(sampler_bank.voices.len()-1);
                            let voice_name = sampler_bank.voices[voice_index].as_str();
                            let default_sampler = sampler_bank.samplers.iter().find(|&n| n.name == voice_name).unwrap();

                            let note_sample = default_sampler.get_sample(
                                sample_rate as u16, midi_note.as_int(), note.samples_played, output_channel as u8)
                                    .unwrap_or(0.0);

                            let attenuated = note_sample * (note.velocity as f32 / 127.0);
                            sample += attenuated;
                            
                            // Increment the amount of samples the note has played.
                            note.samples_played += 1;
                        });

                        producer.push(sample);
                    }

                    samples_produced += 1;
                }

                // Reset the time.
                samples_processed += samples_produced;

                log::info!("Produced {} samples, buffer has {} samples remaining.", samples_produced, producer.remaining());

                // Now process all of the samples up to the point just before the event received above.
                match received {
                    Err(RecvTimeoutError) => (), // Ok, we expect this.
                    Err(e) => println!("error: {}", e),
                    Ok(event) => {
                        // Handle the event we just got and any others that we got at the same time.
                        handle_event(&mut tracks, event);
                        for event in receiver.try_iter() {
                            handle_event(&mut tracks, event);
                        }
                    }
                }
            }
        });

        thread
    }
}

impl Connection for SamplerSynth {
    fn play(&mut self, event: nodi::MidiEvent) -> bool {
        self.message_sender.send(event);
        true
    }
}