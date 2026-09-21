//! A pipe-backed [`CustomAudioDevice`] for ringrtc.
//!
//! Instead of driving a real host audio device, this device exchanges raw PCM
//! with an external process over a Unix domain stream socket. It implements
//! ringrtc's [`CustomAudioDevice`] extension point, so all socket and buffering
//! logic lives here in the tunnel crate rather than inside ringrtc.
//!
//! ## Stream contract
//!
//! A single bidirectional Unix `SOCK_STREAM` socket carries both directions of
//! 48 kHz, mono, signed 16-bit little-endian PCM in 10 ms windows:
//!
//! * **Playout** (audio *received* from the remote peer): pulled from WebRTC via
//!   [`AudioTransport::pull_playout`] and **written** to the peer socket.
//! * **Recording** (audio *sent* to the remote peer): **read** from the peer
//!   socket and handed to WebRTC via [`AudioTransport::push_recording`].
//!
//! This device is the socket *listener*; the external process connects to it.
//! Before a peer connects (or after it disconnects), playout is buffered/dropped
//! and recording is padded with silence so WebRTC keeps running.

use std::{
    io::{ErrorKind, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use log::{error, info, warn};
use ringrtc::webrtc::audio_device_module::{
    AudioControl, AudioControlEvent, AudioTransport, CustomAudioDevice,
};

/// PCM sample rate exchanged over the pipe.
const SAMPLE_FREQUENCY: u32 = 48_000;
/// Number of channels exchanged over the pipe.
const NUM_CHANNELS: u32 = 1;
/// Length of each PCM window exchanged, in milliseconds.
const WINDOW_MS: u64 = 10;
/// Samples per 10 ms window (WebRTC always works in 10 ms windows).
const WEBRTC_WINDOW: usize = SAMPLE_FREQUENCY as usize / 100;
/// Bytes per PCM sample (signed 16-bit).
const BYTES_PER_SAMPLE: usize = std::mem::size_of::<i16>();
/// Reported playout delay (ms). The pipe has no hardware buffer, so this is a
/// small fixed value that keeps WebRTC's timing math sane.
const PIPE_PLAYOUT_DELAY_MS: u16 = WINDOW_MS as u16;

/// Synthetic device name reported through enumeration.
const DEVICE_NAME: &str = "Signal Call Tunnel Pipe";

/// A pipe-backed audio device. Binds its listening socket at construction so
/// failures surface before the call starts and so the socket exists by the time
/// the parent reports its path to the remote peer.
pub struct PipeAudioDevice {
    socket_path: PathBuf,
    listener: UnixListener,
}

impl PipeAudioDevice {
    /// Bind a Unix stream socket at `socket_path` (replacing any stale file).
    pub fn new(socket_path: PathBuf) -> std::io::Result<Self> {
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path)?;
        listener.set_nonblocking(true)?;
        info!("pipe audio: listening on {}", socket_path.display());
        Ok(Self {
            socket_path,
            listener,
        })
    }
}

impl Drop for PipeAudioDevice {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

impl CustomAudioDevice for PipeAudioDevice {
    fn device_name(&self) -> String {
        DEVICE_NAME.to_string()
    }

    fn playout_delay_ms(&self) -> u16 {
        PIPE_PLAYOUT_DELAY_MS
    }

    fn run(&self, transport: AudioTransport, control: AudioControl) {
        let playing = Arc::new(AtomicBool::new(false));
        let recording = Arc::new(AtomicBool::new(false));
        let send_to_webrtc = Arc::new(AtomicBool::new(true));
        let terminate = Arc::new(AtomicBool::new(false));

        // Translate control events into shared state on a dedicated thread so
        // the clock loop below can run on a fixed cadence without blocking.
        let control_thread = {
            let playing = playing.clone();
            let recording = recording.clone();
            let send_to_webrtc = send_to_webrtc.clone();
            let terminate = terminate.clone();
            thread::spawn(move || {
                while let Some(event) = control.recv() {
                    match event {
                        AudioControlEvent::StartPlayout => playing.store(true, Ordering::SeqCst),
                        AudioControlEvent::StopPlayout => playing.store(false, Ordering::SeqCst),
                        AudioControlEvent::StartRecording {
                            send_to_webrtc: send,
                        } => {
                            send_to_webrtc.store(send, Ordering::SeqCst);
                            recording.store(true, Ordering::SeqCst);
                        }
                        AudioControlEvent::StopRecording => {
                            recording.store(false, Ordering::SeqCst)
                        }
                        AudioControlEvent::Terminate => break,
                        _ => {}
                    }
                }
                // Either we saw Terminate or the channel closed; both mean stop.
                terminate.store(true, Ordering::SeqCst);
            })
        };

        self.clock_loop(&transport, &playing, &recording, &send_to_webrtc, &terminate);

        if let Err(e) = control_thread.join() {
            error!("pipe audio: control thread join failed: {:?}", e);
        }
    }
}

impl PipeAudioDevice {
    /// Run the realtime 10 ms PCM exchange until `terminate` is set.
    fn clock_loop(
        &self,
        transport: &AudioTransport,
        playing: &AtomicBool,
        recording: &AtomicBool,
        send_to_webrtc: &AtomicBool,
        terminate: &AtomicBool,
    ) {
        let tick = Duration::from_millis(WINDOW_MS);
        let bytes_per_window = WEBRTC_WINDOW * BYTES_PER_SAMPLE;
        // Cap buffered audio at ~80 ms each way to bound added latency.
        let max_buffered = bytes_per_window * 8;

        let mut stream: Option<UnixStream> = None;
        let mut playout_buf: Vec<u8> = Vec::with_capacity(max_buffered);
        let mut capture_buf: Vec<u8> = Vec::with_capacity(max_buffered);
        let mut next_tick = Instant::now();

        while !terminate.load(Ordering::SeqCst) {
            next_tick += tick;

            if stream.is_none() {
                match self.listener.accept() {
                    Ok((peer, _addr)) => {
                        if let Err(e) = peer.set_nonblocking(true) {
                            warn!("pipe audio: failed to set peer non-blocking: {}", e);
                        }
                        info!("pipe audio: peer connected");
                        playout_buf.clear();
                        capture_buf.clear();
                        stream = Some(peer);
                    }
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(e) => warn!("pipe audio: accept failed: {}", e),
                }
            }

            // Playout: pull 10 ms from WebRTC and queue it for the peer.
            if playing.load(Ordering::SeqCst) {
                let play = transport.pull_playout(WEBRTC_WINDOW, NUM_CHANNELS, SAMPLE_FREQUENCY);
                if stream.is_some() {
                    for sample in &play {
                        playout_buf.extend_from_slice(&sample.to_le_bytes());
                    }
                    cap_buffer(&mut playout_buf, max_buffered);
                }
            }
            if let Some(peer) = stream.as_mut()
                && !playout_buf.is_empty()
                && let Err(e) = drain_playout(peer, &mut playout_buf)
            {
                warn!("pipe audio: playout write failed, dropping peer: {}", e);
                stream = None;
            }

            // Recording: read whatever the peer sent, then deliver one window
            // (padding silence) to WebRTC.
            if let Some(peer) = stream.as_mut()
                && let Err(e) = fill_capture(peer, &mut capture_buf, max_buffered)
            {
                if e.kind() == ErrorKind::UnexpectedEof {
                    info!("pipe audio: peer disconnected");
                } else {
                    warn!("pipe audio: capture read failed: {}", e);
                }
                stream = None;
            }
            if recording.load(Ordering::SeqCst) && send_to_webrtc.load(Ordering::SeqCst) {
                let samples = take_window(&mut capture_buf);
                transport.push_recording(
                    samples,
                    NUM_CHANNELS,
                    SAMPLE_FREQUENCY,
                    Duration::from_millis(u64::from(PIPE_PLAYOUT_DELAY_MS)),
                );
            }

            let now = Instant::now();
            if next_tick > now {
                thread::sleep(next_tick - now);
            } else {
                // We fell behind; reset to avoid a runaway catch-up loop.
                next_tick = now;
            }
        }
    }
}

/// Drop whole samples from the front of `buf` so it never exceeds `max`.
fn cap_buffer(buf: &mut Vec<u8>, max: usize) {
    if buf.len() > max {
        let mut overflow = buf.len() - max;
        overflow -= overflow % BYTES_PER_SAMPLE;
        buf.drain(..overflow);
    }
}

/// Write as much of `buf` as the non-blocking socket accepts, preserving sample
/// alignment by only removing fully written bytes.
fn drain_playout(stream: &mut UnixStream, buf: &mut Vec<u8>) -> std::io::Result<()> {
    let mut written = 0;
    while written < buf.len() {
        match stream.write(&buf[written..]) {
            Ok(0) => break,
            Ok(n) => written += n,
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => {
                buf.drain(..written);
                return Err(e);
            }
        }
    }
    buf.drain(..written);
    Ok(())
}

/// Drain all currently available bytes from the non-blocking socket into `buf`.
///
/// Returns [`ErrorKind::UnexpectedEof`] when the peer has closed the connection.
fn fill_capture(stream: &mut UnixStream, buf: &mut Vec<u8>, max: usize) -> std::io::Result<()> {
    let mut tmp = [0u8; 4096];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => {
                return Err(std::io::Error::from(ErrorKind::UnexpectedEof));
            }
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                cap_buffer(buf, max);
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

/// Remove and return one `WEBRTC_WINDOW`-sample window from `buf`, padding the
/// tail with silence when fewer than a full window of samples is buffered.
fn take_window(buf: &mut Vec<u8>) -> Vec<i16> {
    let mut samples = vec![0i16; WEBRTC_WINDOW];
    let available_samples = (buf.len() / BYTES_PER_SAMPLE).min(WEBRTC_WINDOW);
    for (i, sample) in samples.iter_mut().enumerate().take(available_samples) {
        let lo = buf[i * BYTES_PER_SAMPLE];
        let hi = buf[i * BYTES_PER_SAMPLE + 1];
        *sample = i16::from_le_bytes([lo, hi]);
    }
    buf.drain(..available_samples * BYTES_PER_SAMPLE);
    samples
}
