use std::{path::Path, path::PathBuf, fs::{File}, io::Read, thread::{self, JoinHandle}, sync::Arc, collections::HashMap, num::ParseIntError};
use std::{io};
use std::sync::mpsc::{Receiver, channel, Sender};

use midly::{Smf, TrackEvent, num::u28};
use nodi::{timers::Ticker, midly::{Format, self, Header}, Sheet, Player, Connection, Timer, Moment, Event};
use ringbuf::RingBuffer;

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

pub enum Command {
    Pause,
    Stop,
    Play,
    NewFromFile(PathBuf),
    NewFromBuf(Vec<u8>)
}

#[derive(Debug)]
pub enum MidiPlayerError {
    IoError(io::Error),
    MidiLoadError(nodi::midly::Error),
    MidiTimeError(nodi::timers::TimeFormatError)
}

impl From<io::Error> for MidiPlayerError {
    fn from(e: io::Error) -> Self {
        Self::IoError(e)
    }
}

impl From<nodi::midly::Error> for MidiPlayerError {
    fn from(e: nodi::midly::Error) -> Self {
        Self::MidiLoadError(e)
    }
}

impl From<nodi::timers::TimeFormatError> for MidiPlayerError {
    fn from(e: nodi::timers::TimeFormatError) -> Self {
        Self::MidiTimeError(e)
    }
}

pub struct MidiPlayer {
    com_sender: Sender<Command>,
    player_thread: JoinHandle<()>,
}  

impl MidiPlayer {
    pub fn new<C: Connection + Send + 'static>(midi_connection: C) -> Result<Self, MidiPlayerError> {
        // Communication
        let (com_sender, com_receiver) = channel();
        let thread_handle = Self::create_player_thread(com_receiver, midi_connection);

        let midi_player = Self {
            com_sender,
            player_thread: thread_handle
        };
        Ok(midi_player)
    }

    fn actual_load_from_file(filepath: &Path, timer: &mut Ticker, sheet: &mut Option<Sheet>) -> Result<(), MidiPlayerError> {
        log::info!("Loading new midi file from filepath {}", filepath.display());

        let mut file = File::open(filepath)?;
        let mut midi_bytes = Vec::new();
        file.read_to_end(&mut midi_bytes)?;

        Self::actual_load_from_buf(&midi_bytes, timer, sheet)
    }

    fn actual_load_from_buf(buf: &Vec<u8>, timer: &mut Ticker, sheet: &mut Option<Sheet>) -> Result<(), MidiPlayerError> {
        let smf = Smf::parse(&buf)?;
        *timer = Ticker::try_from(smf.header.timing)?;
        *sheet = Some(match smf.header.format {
            Format::SingleTrack | Format::Sequential => Sheet::sequential(&smf.tracks),
            Format::Parallel => Sheet::parallel(&smf.tracks),
        });

        Ok(())
    }

    fn process_moment<T: Timer, C: Connection>(timer: &mut T, connection: &mut C, moment: &Moment) {
        for event in &moment.events {
            match event {
                Event::Tempo(val) => {
                    log::info!("tempo changed {}", *val); 
                    timer.change_tempo(*val)
                },
                Event::Midi(msg) => {
                    connection.play(*msg);
                },
                _ => (),
            }
        }

        //log::info!("{:?}", moment);
    }

    fn create_player_thread<C: Connection + Send + 'static>(receiver: Receiver<Command>, mut connection: C) -> JoinHandle<()> {
        let thread = thread::spawn(move || {
            log::info!("Starting midi player...");
            let mut timer = Ticker::new(0);
            let mut sheet = None;
            let mut moment_index = 0;
            let mut paused = true;
            let mut looping = false;

            let mut counter = 0;

            loop {
                // Get commands from outside the thread if there are any.
                for event in receiver.try_iter() {
                    let command_result = match event {
                        Command::NewFromFile(filepath) => { 
                            Self::actual_load_from_file(&filepath, &mut timer, &mut sheet)
                        },
                        Command::NewFromBuf(buf) => {
                            Self::actual_load_from_buf(&buf, &mut timer, &mut sheet)
                        },
                        Command::Pause => Ok(()),
                        Command::Stop => Ok(()),
                        Command::Play => {
                            // If there is a sheet (a song), then set the moment to the beginning.
                            moment_index = 0;
                            paused = false; // Unpause if it we were paused.
                            Ok(())
                        }
                    };
                    command_result.unwrap();
                }

                if paused {
                    continue;
                }

                // Iterate through the moments in the sheet and handle them.
                // If there is an iterator, go to the next item and process it.
                if let Some(sheet) = sheet.as_ref() {
                    let moment = &sheet[moment_index];

                    if !moment.is_empty() {
                        // Sleep a tick.
                        timer.sleep(counter);
                        log::info!("Slept {} ticks, {} seconds", counter, timer.sleep_duration(counter).as_secs_f32());
                        counter = 0;

                        // Process the moment.
                        Self::process_moment(&mut timer, &mut connection, moment);
                    }

                    // Increment to the next moment.
                    if moment_index < sheet.len() - 1 {
                        moment_index += 1;
                    } else if looping {
                        moment_index = 0;
                    } else {
                        paused = true;
                    }

                    counter += 1;
                }
            }
        });

        thread
    }

    pub fn load_from_file(&mut self, filepath: &Path) {
        self.com_sender.send(Command::NewFromFile(filepath.to_owned()));
    }

    pub fn load_from_buf(&mut self, buf: &Vec<u8>) {
        self.com_sender.send(Command::NewFromBuf(buf.to_owned()));
    }

    pub fn play(&mut self) {
        self.com_sender.send(Command::Play);
    }

    pub fn pause(&mut self) {
        self.com_sender.send(Command::Pause);
    }

    pub fn stop(&mut self) {
        self.com_sender.send(Command::Stop);
    }
}