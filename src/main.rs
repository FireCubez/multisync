use websocket::sync::{Server, Writer};
use websocket::OwnedMessage;

use opus::Decoder;

use slotmap::{DefaultKey, SlotMap};

use hound::{WavReader, WavWriter, WavSpec};
use ringbuf::{Consumer, Producer, RingBuffer};
use rodio::{Source, Sink};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering, AtomicUsize};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use rodio::source::SineWave;
use std::fs::File;
use std::io::{Write, BufWriter};

pub enum Backing {
	File(Vec<u8>),
	Metronome(f32, usize)
}

impl Backing {
	pub fn decode(&self) -> (Vec<i16>, u16) {
		match self {
			Self::File(f) => decode(f).unwrap_or_else(|| (Vec::new(), 1)),
			Self::Metronome(bpm, sig) => {
				let up_tick: Vec<i16> = match std::fs::read("./MetronomeUp.raw") {
					Ok(x) => x,
					Err(_) => include_bytes!("../MetronomeUp.raw").to_vec()
				}.chunks(2).map(|x| i16::from_le_bytes([x[0], x[1]])).collect();
				let tick: Vec<i16> = match std::fs::read("./Metronome.raw") {
					Ok(x) => x,
					Err(_) => include_bytes!("../Metronome.raw").to_vec()
				}.chunks(2).map(|x| i16::from_le_bytes([x[0], x[1]])).collect();
				let mut output = Vec::new();
				for i in 0..10000 {
					let downbeat = i % sig == 0;
					let sample = ((48000 * 60 * i) as f32 / bpm) as usize;
					output.resize(sample, 0);
					output.extend_from_slice(if downbeat {
						&up_tick
					} else {
						&tick
					});
				}
				(output, 1)
			}
		}
	}
}

pub struct Room {
	pub users: SlotMap<DefaultKey, Arc<Mutex<User>>>,
	pub bpm: u32,
	pub counts: u32,
	pub min_buf_len: usize,
	pub backing: Backing,
	pub audience_hears: bool,
}

impl Room {
	pub fn update_info(&mut self, backing: bool) {
		let mut list = String::new();
		for (_, user) in self.users.iter() {
			if !list.is_empty() {
				list.push_str(", ");
			}
			list.push_str(&user.lock().unwrap().username);
		}
		self.users.retain(|_, user| {
			let mut user = user.lock().unwrap();
			if user
				.sender
				.send_message(&OwnedMessage::Text(format!("onl:{}", list)))
				.is_err()
			{
				return false;
			}
			if user
				.sender
				.send_message(&OwnedMessage::Text(format!("bpm:{}", self.bpm)))
				.is_err()
			{
				return false;
			}
			if user
				.sender
				.send_message(&OwnedMessage::Text(format!("cnt:{}", self.counts)))
				.is_err()
			{
				return false;
			}
			if user
				.sender
				.send_message(&OwnedMessage::Text(format!("buf:{}", self.min_buf_len)))
				.is_err()
			{
				return false;
			}
			if user
				.sender
				.send_message(&OwnedMessage::Text(format!("abk:{}", self.audience_hears)))
				.is_err()
			{
				return false;
			}
			if backing {
				match &self.backing {
					Backing::File(f) => {
						if user
							.sender
							.send_message(&OwnedMessage::Text(String::from("bck:")))
							.is_err()
						{
							return false;
						}
						if user
							.sender
							.send_message(&OwnedMessage::Binary(f.clone()))
							.is_err()
						{
							return false;
						}
					}
					Backing::Metronome(bpm, sig) => {
						if user
							.sender
							.send_message(&OwnedMessage::Text(
								format!("met:{}:{}", bpm, sig)
							))
							.is_err()
						{
							return false;
						}
					}
				}
			}
			true
		});
	}
}

pub struct User {
	sender: Writer<TcpStream>,
	username: String,
	key: DefaultKey,
	offset: usize,
	consumer_idx: usize,
	producer_sender: Sender<(Producer<i16>, File)>,
}

fn decode_mp3(bytes: &[u8]) -> Result<(Vec<i16>, u16), minimp3::Error> {
	let mut decoder = minimp3::Decoder::new(bytes);
	let mut data = Vec::new();
	let mut channels = None;
	loop {
		match decoder.next_frame() {
			Ok(f) => {
				if let Some(chs) = channels {
					assert_eq!(f.channels, chs as usize);
				} else {
					channels = Some(f.channels as u16);
				}
				data.extend(f.data);
			}
			Err(minimp3::Error::Eof) => break,
			Err(e) => return Err(e),
		}
	}
	Ok((data, channels.unwrap_or(1)))
}

fn decode(bytes: &[u8]) -> Option<(Vec<i16>, u16)> {
	if bytes.is_empty() {
		return Some((Vec::new(), 1));
	}
	Some(if let Ok(reader) = WavReader::new(bytes) {
		let ch = reader.spec().channels;
		(
			reader.into_samples().map(Result::unwrap).collect(),
			ch
		)
	} else if let Ok(t) = decode_mp3(bytes) {
		t
	} else {
		return None;
	})
}

pub struct Stream {
	cons: Consumer<i16>,
	lag: usize,
}

impl Stream {
	pub fn new(cons: Consumer<i16>) -> Self {
		Self { cons, lag: 0 }
	}

	pub fn pop(&mut self) -> i16 {
		while self.lag != 0 && self.cons.pop().is_some() {
			self.lag -= 1;
		}

		if let Some(x) = self.cons.pop() {
			x
		} else {
			self.lag += 1;
			0
		}
	}
}

pub struct MultisyncSource {
	consumers: Vec<Stream>,
	channels: u16,
	current_channel: u16,
	last_mono: i16,
	override_controller: Arc<AtomicUsize>,
	backing: Vec<i16>,
	backing_index: i32,
	consumer_outputs: Vec<WavWriter<BufWriter<File>>>,
	backing_output: WavWriter<BufWriter<File>>,
	total_output: WavWriter<BufWriter<File>>,
}

impl Source for MultisyncSource {
	fn current_frame_len(&self) -> Option<usize> {
		None
	}

	fn channels(&self) -> u16 {
		self.channels
	}

	fn sample_rate(&self) -> u32 {
		48000
	}

	fn total_duration(&self) -> Option<Duration> {
		None
	}
}

impl Iterator for MultisyncSource {
	type Item = i16;

	fn next(&mut self) -> Option<i16> {
		let mut overriden = false;
		if self.current_channel == 0 {
			let mut ovr = self.override_controller.load(Ordering::Relaxed);
			if ovr != usize::MAX && ovr >= 0x1000000 {
				self.override_controller.store(0, Ordering::Relaxed);
				println!("sync override: {:x}", ovr);
				ovr -= 0x1000000;
				let idx = ovr & 0xFF;
				self.consumers[idx].lag += ovr >> 8 & 0xFFFF;
			} else if ovr < self.consumers.len() {
				overriden = true;
				self.last_mono = self.consumers[ovr].pop();
			} else {
				//self.last_mono = self.consumers.iter_mut().map(|x| x.pop().unwrap_or_default()).sum();
				self.last_mono = 0;
				for (i, cons) in self.consumers.iter_mut().enumerate() {
					let s = cons.pop();
					self.last_mono += s;
					self.consumer_outputs[i].write_sample(s).unwrap();
				}
			}
			//println!("setting last mono {}", self.last_mono);
		}
		let mut sample = self.last_mono;
		let mut wrote_backing = false;
		if !overriden && self.backing_index >= 0 {
			if let Some(&b) = self.backing.get(self.backing_index as usize) {
				sample = sample.saturating_add(b);
				wrote_backing = true;
				self.backing_output.write_sample(b).unwrap();
			}
		}
		if !wrote_backing {
			self.backing_output.write_sample(0).unwrap();
		}
		self.backing_index += 1;
		self.current_channel += 1;
		if self.current_channel == self.channels {
			self.current_channel = 0;
		}
		self.total_output.write_sample(sample).unwrap();
		//println!("sample: {}", sample);
		Some(sample)
	}
}

pub enum AudioMessage {
	StartRecording(Vec<Stream>, Vec<usize>, u16, Vec<i16>, i32),
	StopRecording,
	Override(usize),
	MinBuf(usize),
	Beep
}

const DEFAULT_MIN_BUF_LEN: usize = 48000 * 5;

fn audio_handler(audio_recv: Receiver<AudioMessage>) {
	let (_stream, handle) = rodio::OutputStream::try_default().unwrap();
	let mut sink = None;
	let mut overrider = None;
	let mut minbuf = DEFAULT_MIN_BUF_LEN;
	loop {
		match audio_recv.recv().unwrap() {
			AudioMessage::StartRecording(mut consumers, offsets, channels, backing, backing_index) => {
				if sink.is_some() { continue; }
				println!("starting {}", channels);
				
				for consumer in &consumers {
					while consumer.cons.len() < minbuf {
						std::thread::sleep(Duration::from_millis(100));
					}
				}

				let mut consumer_outputs = Vec::new();
				let mut i = 0;
				for (consumer, offset) in consumers.iter_mut().zip(offsets) {
					consumer.lag += offset;
					consumer_outputs.push(WavWriter::create(format!("./cons{}.wav", i), WavSpec {
						bits_per_sample: 16,
						channels: 1,
						sample_format: hound::SampleFormat::Int,
						sample_rate: 48000
					}).unwrap());
					i += 1;
				}
				let backing_output = WavWriter::create("./backing.wav", WavSpec {
					channels,
					sample_rate: 48000,
					bits_per_sample: 16,
					sample_format: hound::SampleFormat::Int
				}).unwrap();
				let total_output = WavWriter::create("./total.wav", WavSpec {
					channels,
					sample_rate: 48000,
					bits_per_sample: 16,
					sample_format: hound::SampleFormat::Int
				}).unwrap();
				let s = Sink::try_new(&handle).unwrap();
				let override_controller = Arc::new(AtomicUsize::new(usize::MAX));
				s.append(MultisyncSource {
					consumers,
					channels,
					current_channel: 0,
					last_mono: 0,
					override_controller: override_controller.clone(),
					backing,
					backing_index,
					consumer_outputs,
					backing_output,
					total_output,
				});
				overrider = Some(override_controller);
				sink = Some(s);
			}
			AudioMessage::StopRecording => {
				sink = None;
				overrider = None;
			}
			AudioMessage::Override(idx) => {
				if let Some(o) = &overrider {
					o.store(idx, Ordering::Relaxed);
				}
			}
			AudioMessage::MinBuf(min) => {
				minbuf = min;
			}
			AudioMessage::Beep => {
				handle.play_raw(SineWave::new(1000.0).take_duration(Duration::from_millis(200))).unwrap();
			}
		}
	}
}

fn main() {
	let server = Server::bind("0.0.0.0:3000").unwrap();
	let recording = Arc::new(AtomicBool::new(false));
	let room = Arc::new(Mutex::new(Room {
		users: SlotMap::new(),
		bpm: 60,
		counts: 4,
		min_buf_len: DEFAULT_MIN_BUF_LEN,
		backing: Backing::File(Vec::new()),
		audience_hears: true,
	}));
	let audio_send = {
		let (audio_send, audio_recv) = mpsc::channel();
		std::thread::spawn(move || audio_handler(audio_recv));
		audio_send
	};
	for request in server {
		let request = match request {
			Ok(x) => x,
			Err(e) => {
				println!("{:?}", e);
				continue;
			}
		};
		let client = request.accept().unwrap();
		let (mut receiver, mut sender) = client.split().unwrap();
		let mut room_lock = room.lock().unwrap();
		match &room_lock.backing {
			Backing::File(f) => {
				let _ = sender.send_message(&OwnedMessage::Binary(f.clone()));
			}
			Backing::Metronome(bpm, sig) => {
				let _ = sender.send_message(&OwnedMessage::Text(
					format!("met:{}:{}", bpm, sig)
				));
			}
		}
		let (producer_sender, producer_receiver) = mpsc::channel();
		let mut user = Arc::new(Mutex::new(User {
			sender,
			username: String::from("(user is connecting)"),
			key: DefaultKey::default(),
			offset: 0,
			consumer_idx: 0,
			producer_sender,
		}));
		room_lock.users.insert_with_key(|key| {
			Arc::get_mut(&mut user).unwrap().get_mut().unwrap().key = key;
			user.clone()
		});
		room_lock.update_info(false);
		drop(room_lock);
		let room = room.clone();
		let audio_send = audio_send.clone();
		let recording = recording.clone();
		std::thread::spawn(move || {
			println!("client connected");
			let mut producer = None;
			let mut labels = None;
			let mut decoder = Decoder::new(48000, opus::Channels::Mono).unwrap();
			let mut sending_backing = false;
			let mut packet_time = None;
			let mut current_time = 0;
			let mut using_opus = false;
			for msg in receiver.incoming_messages() {
				//println!("message");
				let msg = match msg {
					Ok(x) => x,
					Err(e) => {
						println!("disconnecting client due to error: {:?}", e);
						break;
					}
				};
				match msg {
					OwnedMessage::Close(_) => {
						let mut user = user.lock().unwrap();
						println!("user {} disconnected", user.username);
						let _ = user.sender.send_message(&OwnedMessage::Close(None));
						room.lock().unwrap().users.remove(user.key);
						break;
					}
					OwnedMessage::Ping(p) => {
						let mut user = user.lock().unwrap();
						if user.sender.send_message(&OwnedMessage::Pong(p)).is_err() {
							room.lock().unwrap().users.remove(user.key);
						}
					}
					OwnedMessage::Text(t) => {
						if t.len() >= 4 {
							match &t[..4] {
								"usr:" => {
									let mut user = user.lock().unwrap();
									user.username = t[4..].to_string();
									let mut room = room.lock().unwrap();
									drop(user);
									room.update_info(false);
								}
								"bpm:" => {
									let bpm = if let Ok(x) = t[4..].parse::<u32>() {
										x
									} else {
										continue;
									};
									let mut room = room.lock().unwrap();
									room.bpm = bpm;
									room.update_info(false);
								}
								"cnt:" => {
									let counts = if let Ok(x) = t[4..].parse::<u32>() {
										x
									} else {
										continue;
									};
									let mut room = room.lock().unwrap();
									room.counts = counts;
									room.update_info(false);
								}
								"buf:" => {
									let buflen = if let Ok(x) = t[4..].parse::<usize>() {
										x
									} else {
										continue;
									};
									let mut room = room.lock().unwrap();
									room.min_buf_len = buflen;
									room.update_info(false);
									audio_send.send(AudioMessage::MinBuf(buflen)).unwrap();
								},
								"met:" => {
									let mut parts = t[4..].split(':');
									let bpm = if let Some(x) = parts.next().and_then(|x| x.parse::<f32>().ok()) {
										x
									} else {
										continue;
									};
									let sig = if let Some(x) = parts.next().and_then(|x| x.parse::<usize>().ok()) {
										x
									} else {
										continue;
									};
									let mut room = room.lock().unwrap();
									room.backing = Backing::Metronome(bpm, sig);
									room.update_info(true);
								}
								"off:" => {
									let offset = if let Ok(x) = t[4..].parse::<usize>() {
										x
									} else {
										continue;
									};
									user.lock().unwrap().offset = offset;
								}
								"srt:" => {
									if recording.swap(true, Ordering::Relaxed) { continue; }
									let mut room = room.lock().unwrap();
									let count_delay = 48000 * 60 * room.counts / room.bpm;
									let mut consumers = Vec::new();
									let mut offsets = Vec::new();
									room.users.retain(|_, user| {
										let (prod, cons) = RingBuffer::new(0x1_000_000).split();
										let mut user = user.lock().unwrap();
										let labels = File::create(format!("{}.txt", user.username)).unwrap();
										if user.producer_sender.send((prod, labels)).is_ok() {
											user.consumer_idx = consumers.len();
											consumers.push(Stream::new(cons));
											offsets.push(user.offset);
											true
										} else {
											false
										}
									});
									let (backing, channels) = if room.audience_hears {
										room.backing.decode()
									} else {
										(Vec::new(), 1)
									};
									let backing_index = -(count_delay as i32) * channels as i32;
									room.users.retain(|_, user| {
										let mut user = user.lock().unwrap();
										user.sender
											.send_message(&OwnedMessage::Text(format!(
												"srt:{}",
												count_delay
											)))
											.is_ok()
									});
									audio_send
										.send(AudioMessage::StartRecording(
											consumers,
											offsets,
											channels,
											backing,
											backing_index,
										))
										.unwrap();
								}
								"stp:" => {
									if !recording.swap(false, Ordering::Relaxed) { continue; }
									audio_send.send(AudioMessage::StopRecording).unwrap();
									room.lock().unwrap().users.retain(|_, user| {
										user.lock().unwrap().sender.send_message(&OwnedMessage::Text(
											String::from("stp:")
										)).is_ok()
									});
									packet_time = None;
									labels = None;
									current_time = 0;
								}
								"bck:" => {
									sending_backing = true;
								}
								"emg:" => {
									let mut parts = t[4..].split(':');
									let username = parts.next().unwrap();
									let offset = parts.next().map(|x| x.parse::<usize>().unwrap()).unwrap_or_default();
									let room = room.lock().unwrap();
									let target = room.users.iter().find_map(|(_, x)| {
										let x = x.lock().unwrap();
										if x.username == username { Some(x) } else { None }
									});
									let locked_user;
									let target = match target {
										Some(x) => x,
										None => {
											locked_user = user.lock().unwrap();
											locked_user
										}
									};
									// emergency failsafe
									audio_send.send(AudioMessage::Override(target.consumer_idx + offset)).unwrap();
								}
								"tim:" => {
									if let Ok(x) = t[4..].parse::<usize>() {
										packet_time = Some(x);
									} else {
										continue;
									};
								}
								"ops:" => {
									using_opus = true;
								}
								"nps:" => {
									using_opus = false;
								}
								"abk:" => {
									let mut room = room.lock().unwrap();
									room.audience_hears = true;
									room.update_info(false);
								},
								"nbk:" => {
									let mut room = room.lock().unwrap();
									room.audience_hears = false;
									room.update_info(false);
								}
								_ => {}
							}
						}
					}
					OwnedMessage::Binary(b) => {
						let mut room = room.lock().unwrap();
						if recording.load(Ordering::Relaxed) {
							let (mut samples, len) = if using_opus {
								let mut samples = vec![0; decoder.get_nb_samples(&b).unwrap()];
								let len = decoder.decode(&b, &mut samples, false).unwrap();
								if len != samples.len() {
									println!("len ({}) != allocated ({})", len, samples.len());
								}
								(samples, len)
							} else {
								let s: Vec<i16> = b.chunks(4).map(|x| rodio::cpal::Sample::to_i16(&f32::from_le_bytes([x[0], x[1], x[2], x[3]]))).collect();
								let len = s.len();
								(s, len)
							};
							if let Ok((p, l)) = producer_receiver.try_recv() {
								producer = Some(p);
								labels = Some(l);
							} else if producer.is_none() { // if we don't have a producer from before
								let (p, l) = producer_receiver.recv().unwrap();
								producer = Some(p);
								labels = Some(l);
							}
							if let Some(ptime) = packet_time {
								if ptime > current_time {
									// if after adding these samples, our time is
									// still falling behind the real time, we need
									// to compensate by adding silence
									println!("compensating to {} from {} = {}", ptime, current_time, ptime - current_time);
									for _ in 0..(ptime - current_time) {
										producer.as_mut().unwrap().push(0).unwrap();
									}
								} else if ptime < current_time {
									// if after adding these samples our time
									// is too much then we need to compensate by
									// not adding as much
									println!("compensating to {} from {} = -{}", ptime, current_time, current_time - ptime);
									let end = (current_time - ptime).min(samples.len());
									samples.drain(0..end);
								}
								writeln!(labels.as_mut().unwrap(), "{}\t{}\t{}", ptime as f64 / 48000.0, ptime as f64 / 48000.0, ptime).unwrap();
								packet_time = None;
							}

							current_time += samples.len();

							producer.as_mut().unwrap().push_slice(&samples[..len]);
						} else if sending_backing {
							sending_backing = false;
							room.backing = Backing::File(b);
							room.update_info(true);
						}
					}
					OwnedMessage::Pong(_) => {}
				}
			}
		});
	}
}
