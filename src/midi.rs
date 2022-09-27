use std::{path::Path, path::PathBuf, fs::{File, self}, io::Read, thread::{self, JoinHandle}, sync::{Arc, mpsc::{TryRecvError, RecvError}}, collections::HashMap, num::ParseIntError, time::Duration, rc::Rc};
use std::{io};
use std::sync::mpsc::{Receiver, channel, Sender};

use itertools::Itertools;
use midly::{Smf, TrackEvent, num::u28, TrackEventKind, Track, Timing, MetaMessage, MidiMessage};
use ringbuf::RingBuffer;
use serde::__private::ser::FlatMapSerializer;

const RING_BUF_SIZE: usize = 2000;

pub fn midi_note_to_freq(note: u8) -> f32 {
    const a: f32 = 440.0;
    (a / 32.0) * f32::powf(2.0, (note as f32 - 9.0) / 12.0)
}

#[derive(Debug)]
pub enum NoteNameToMidiError {
    ParseIntError(ParseIntError),
    NameIncorrectLength,
    InvalidNoteName
}

impl From<ParseIntError> for NoteNameToMidiError {
    fn from(e: ParseIntError) -> Self {
        Self::ParseIntError(e)
    }
}

pub fn note_name_to_midi_note(name: &str) -> Result<u8, NoteNameToMidiError> {
    const A0: u8 = 21;
    let (letter_to_check, octave) = match name.len() {
        3 => {
            let o = &name[2..3];
            (&name[0..2], o.parse::<u8>()?)
        },
        2 => {
            let o = &name[1..2];
            (&name[0..1], o.parse::<u8>()?)
        },
        _ => Err(NoteNameToMidiError::NameIncorrectLength)?
    };

    let offset = match letter_to_check {
        "a" => 0,
        "a#" => 1,
        "b" => 2,
        "c" => 3,
        "c#" => 4,
        "d" => 5,
        "d#" => 6,
        "e" => 7,
        "f" => 8,
        "f#" => 9,
        "g" => 10,
        "g#" => 11,
        _ => Err(NoteNameToMidiError::InvalidNoteName)?
    };

    Ok(octave * 12 + offset + A0)
}

// A single frame of audio. Either Mono or Stereo.
pub enum Frame {
    Mono(f32),
    Stereo(f32, f32)
}

// A synthesizer that the midiplayer can use to generate samples.
pub trait Synth {
    fn midi_message(&mut self, channel: usize, message: MidiMessage);
    fn gen_samples(&mut self, output_channel_count: usize, output: &mut [Frame]) -> usize;
}

pub struct MidiSong<'a> {
    smf: Smf<'a>
}

impl<'a> MidiSong<'a> {
    pub fn from_bytes(bytes: &'a Vec<u8>) -> Self {
        let mut song = Self {
            smf: Smf::parse(bytes).unwrap()
        };

        song
    }

    // Found this here: https://codeberg.org/PieterPenninckx/midi-reader-writer/src/branch/main/src/midly_0_5.rs
    // Combines all the tracks into one iterator and adds an offset so that we know the absolute time of the 
    // event.
    pub fn iter(&'a self) -> impl Iterator<Item = (u64, usize, TrackEventKind<'a>)> + 'a {
        let mut track_index = 0;
        self.smf.tracks.iter()
            .map(|t| {
                let mut offset = 0;
                let result = t.iter().map(move |e| {
                    offset += e.delta.as_int() as u64;
                    (offset, track_index, e.kind)
                });
                track_index += 1;
                result
            }).kmerge_by(|(offset1, _, _), (offset2, _, _)| offset1 < offset2)
} 
}

// Allows for samples to be consumed for an audio output device.
pub struct MidiPlayerOutput {
    consumer: ringbuf::Consumer<Frame>
}

impl MidiPlayerOutput {
    pub fn new(consumer: ringbuf::Consumer<Frame>) -> Self {
        Self {
            consumer
        }
    }

    pub fn get_next_frame(&mut self) -> Option<Frame> {
        self.consumer.pop()
    }
}

// The actual player. Runs in a thread and fills a ringbuffer with samples.
pub struct MidiPlayerThread {
    thread_handle: JoinHandle<()>
}

impl MidiPlayerThread {
    pub fn new<S: Synth + Send + 'static>(sample_rate: usize, channel_count: usize, receiver: Receiver<Command>, producer: ringbuf::Producer<Frame>, mut synth: S) -> Self {
        let thread_handle = thread::spawn(move || {
            tracing::info!("MidiPlayer thread started.");

            // A pending command from the last loop. 
            // Is expected to be one of the commands checked in the song block at the start
            // of the outer loop.
            let mut pending_command = None;

            // The bytes for the song that is currently playing. If it is none then no song is loaded.
            let mut song_bytes = None;

            // Whether or not the player is playing.
            let mut playing = false;

            // Whether or not we are looping.
            let mut looping = false;

            // This loop is reset whenever a song is loaded or stopped.
            'outer: loop {
                // Whether or not we are paused.
                let mut paused = false;

                // Until there is a load command, then there will be no song to play.
                // Keep checking for a load commmand before we continue to play it.
                let song = {
                    // Keep trying until we get a song loaded or until the player isn't stopped
                    while song_bytes.is_none() || !playing {
                        // Use the pending_command if it exists, otherwise poll the receiver.
                        let mut command = pending_command.take();
                        if command.is_none() {
                            command = match receiver.recv() {
                                Ok(c) => Some(c),
                                Err(RecvError) => {
                                    tracing::warn!("MidiPlayer receiver was disconnected");
                                    break 'outer
                                }
                            }
                        }
                        
                        // Handle loading a new song or starting the player.
                        // Other commands are only valid whilst a song is playing.
                        match command {
                            Some(Command::NewFromFile(filepath)) => {
                                song_bytes = Some(fs::read(filepath).unwrap());
                            },
                            Some(Command::NewFromBuf(buf)) => {
                                song_bytes = Some(buf);
                            },
                            // In this context play means to start playing the song.
                            Some(Command::Play) => {
                                playing = true;
                            }
                            Some(Command::Loop) => {
                                tracing::info!("Toggling loop to {}", !looping);
                                looping = !looping;
                            }
                            Some(_) => {
                                if song_bytes.is_none() { 
                                    tracing::info!("No midi file is currently loaded! Load a file first!");
                                } else {
                                    tracing::info!("Player is currently stopped, use play to start it.");
                                }
                            },
                            None => ()
                        }
                    }

                    // Parse the song bytes.
                    MidiSong::from_bytes(song_bytes.as_ref().unwrap())
                };

                // Set this flag to true if the song needs unloading.
                // This will be used if a new song is requested to be loaded.
                // It's left alone if the song is stopped.
                let mut unload_song = false;

                // This loop goes through all the events in the midi file and generates samples.
                // Breaking out of it means stopping playback and starting from the beginning (either the same or a new song).
                // Puting this in it's own scope prevents the borrower complaining about song_bytes being modified below this.
                {
                    // // Keep track of how long a tick is.
                    // let tick_length = match song.smf.header.timing {
                    //     Timing::Timecode(fps, subframe) => {
                    //         1.0 / fps.as_f32() / subframe as f32
                    //     },
                    //     Timing::Metrical(tbp) => {
                    //         tbp
                    //     }
                    // }

                    // How many samples until the next event should be processed?
                    let mut samples_until_next_event = 0.0;

                    // How many samples to process per tick?
                    let mut samples_per_tick = 0.0;
                    if let Timing::Timecode(fps, subframe)= song.smf.header.timing {
                        samples_per_tick = (1.0 / fps.as_f32() / subframe as f32) * sample_rate as f32;
                    }

                    let mut event_iter = song.iter();
                    'event_iter: loop {
                        // Parse commands first...
                        for event in receiver.try_iter() {
                            match event {
                                Command::NewFromFile(filepath) => {
                                    pending_command = Some(Command::NewFromFile(filepath));
                                    unload_song = true;
                                    break 'event_iter
                                }
                                Command::NewFromBuf(buf) => {
                                    pending_command = Some(Command::NewFromBuf(buf));
                                    unload_song = true;
                                    break 'event_iter
                                }
                                Command::Pause => {
                                    tracing::info!("Pausing...");
                                    paused = true;
                                },
                                Command::Stop => {
                                    tracing::info!("Stopping...");
                                    playing = false;
                                    break 'event_iter
                                },
                                // In this context play means to unpause.
                                Command::Play => {
                                    paused = false;
                                    tracing::info!("Already playing!");
                                },
                                Command::Loop => {
                                    tracing::info!("Toggling loop to {}", !looping);
                                    looping = !looping;
                                }
                            }
                        }

                        // Process a number of samples
                        let samples_to_process = producer.remaining() / channel_count;

                        // If we are paused, then produced empty samples.
                        if paused {
                            for _ in 0..samples_to_process {
                                match channel_count {
                                    1 => producer.push(Frame::Mono(0.0)),
                                    _ => producer.push(Frame::Stereo(0.0, 0.0))
                                }
                            }
                            continue;
                        }

                        // Get the event.
                        if samples_until_next_event <= 0.0 {
                            match event_iter.next() {
                                None => break,
                                Some((delta, channel, event)) => {
                                    //tracing::info!("{:?}", event);
                                    
                                    // Process the event.
                                    match event {
                                        // Calculate the new samples_per_tick value based on this information.
                                        TrackEventKind::Meta(MetaMessage::Tempo(micro_per_beat)) => {
                                            samples_per_tick = match song.smf.header.timing {
                                                Timing::Metrical(tbp) => {
                                                    tracing::debug!("Tempo change to {} ticks per beat.", tbp);

                                                    // TODO: Make sure this is right?
                                                    ((micro_per_beat.as_int() / tbp.as_int() as u32) as usize * sample_rate) as f32 / 1000000.0
                                                },
                                                _ => samples_per_tick
                                            }
                                        },
                                        TrackEventKind::Meta(_) => (), // No other meta events are important.
                                        TrackEventKind::Midi { channel, message } => {
                                            // Midi messages for our synth.
                                            synth.midi_message(channel.as_int() as usize, message);
                                        },
                                        _ => () // Other messages are not important.
                                    }

                                    // Record how many samples until the next event.
                                    samples_until_next_event = delta as f32 * samples_per_tick;
                                }
                            }
                        }
                    }
                }

                // Unload the song if required.
                if unload_song {
                    song_bytes = None;
                    playing = false;
                }

                // If looping isn't set stop the player.
                if !looping {
                    playing = false;
                }
            }

            // Thread stopped! 
            tracing::info!("MidiPlayer thread ended.");
        });

        Self {
            thread_handle
        }
    }
}

// Commands that can be sent to the midi player thread.
#[derive(Debug, Clone)]
pub enum Command {
    Pause,
    Stop,
    Play,
    Loop,
    NewFromFile(PathBuf),
    NewFromBuf(Vec<u8>)
}

// Allows control over the thread actually playing the midi file.
pub struct MidiPlayerController {
    sender: Sender<Command>
}

impl MidiPlayerController {
    pub fn new(sender: Sender<Command>) -> Self {
        Self {
            sender
        }
    }

    pub fn load_from_file(&mut self, filepath: &Path) {
        self.sender.send(Command::NewFromFile(filepath.to_owned()));
    }

    pub fn load_from_buf(&mut self, buf: &Vec<u8>) {
        self.sender.send(Command::NewFromBuf(buf.to_owned()));
    }

    pub fn play(&mut self) {
        self.sender.send(Command::Play);
    }

    pub fn pause(&mut self) {
        self.sender.send(Command::Pause);
    }

    pub fn stop(&mut self) {
        self.sender.send(Command::Stop);
    }

    pub fn toggle_loop(&mut self) {
        self.sender.send(Command::Loop);
    }
}

// Create a midi player with associated controllers and output.
const BUFFER_LENGTH: f32 = 0.1;
pub fn create_player<S: Synth + Send + 'static>(output_sample_rate: usize, output_channel_count: usize, synth: S) -> (MidiPlayerThread, MidiPlayerController, MidiPlayerOutput) {
    // Create consumer, producer pair to control the player.
    let ring_buf = ringbuf::RingBuffer::new((output_sample_rate as f32 * BUFFER_LENGTH) as usize);
    let (producer, consumer) = ringbuf::RingBuffer::split(ring_buf);

    // Create sender and receiver for controlling the midi player thread.
    let (sender, receiver) = channel();

    let player = MidiPlayerThread::new(output_sample_rate, output_channel_count, receiver, producer, synth);
    let controller = MidiPlayerController::new(sender);
    let output = MidiPlayerOutput::new(consumer);
    (player, controller, output)

}