mod config;
mod control;
mod platform;

use std::io::BufRead;
use std::io::BufReader;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use log::{debug, error, info};

use ringrtc::common::{CallConfig, CallId, CallMediaType, DataMode, DeviceId};
use ringrtc::core::{call_manager::CallManager, signaling};
use ringrtc::lite::http;
use ringrtc::native::{NativeCallContext, NativePlatform, PeerId};
use ringrtc::virtual_audio::VirtualAudioDevicePair;
use ringrtc::webrtc::{
    media::{VideoFrame, VideoSink},
    peer_connection_factory::{AudioConfig, IceServer, PeerConnectionFactory},
};

use crate::config::Config;
use crate::control::{ControlMessage, start_control_channel};
use crate::platform::{
    PlatformEvent, TunnelGroupHandler, TunnelSignalingSender, TunnelStateHandler,
};

/// Dummy video sink that discards all frames.
#[derive(Debug)]
struct NullVideoSink;

impl VideoSink for NullVideoSink {
    fn on_video_frame(&self, _track_id: u32, _frame: VideoFrame) {}
    fn box_clone(&self) -> Box<dyn VideoSink> {
        Box::new(NullVideoSink)
    }
}

/// Dummy HTTP client for CallManager (no SFU needed for 1:1 calls).
#[derive(Clone)]
struct NullHttpClient;

impl http::Delegate for NullHttpClient {
    fn send_request(&self, _request_id: u32, _request: http::Request) {
        // No-op -- no group call SFU requests
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    // Read config from the first line of stdin, keep stdin open for control messages
    let stdin = std::io::stdin();
    let mut stdin_reader = BufReader::new(stdin);
    let mut config_line = String::new();
    stdin_reader
        .read_line(&mut config_line)
        .context("failed to read config from stdin")?;
    let config: Config =
        serde_json::from_str(&config_line).context("failed to parse config JSON")?;

    info!(
        "signal-call-tunnel starting: call_id={}, is_outgoing={}",
        config.call_id, config.is_outgoing
    );

    // Create virtual audio devices (signal-call-tunnel owns their lifecycle).
    // On macOS, BlackHole drivers must be pre-installed with matching names (requires
    // root), so we default to fixed names.  On Linux, PulseAudio virtual sinks are
    // created dynamically, so per-call unique names avoid collisions.
    let input_name = config.input_device_name.clone().unwrap_or_else(|| {
        if cfg!(target_os = "macos") {
            "signal_input".to_string()
        } else {
            format!("signal_input_{}", config.call_id)
        }
    });
    let output_name = config.output_device_name.clone().unwrap_or_else(|| {
        if cfg!(target_os = "macos") {
            "signal_output".to_string()
        } else {
            format!("signal_output_{}", config.call_id)
        }
    });

    let virtual_audio = VirtualAudioDevicePair::new(&input_name, &output_name)?;
    info!(
        "Virtual audio devices: input={}, output={}",
        virtual_audio.input_source(),
        virtual_audio.output_sink()
    );

    // Channel for platform events (signaling + state changes)
    let (event_sender, event_receiver) = mpsc::channel::<PlatformEvent>();

    // Start control channel (stdin for commands, stdout for events)
    let control = start_control_channel(
        stdin_reader,
        virtual_audio.input_source(),
        virtual_audio.output_sink(),
    )?;

    // Show WebRTC logs while debugging
    #[cfg(debug_assertions)]
    ringrtc::webrtc::logging::set_logger(log::LevelFilter::Debug);

    #[cfg(not(debug_assertions))]
    ringrtc::webrtc::logging::set_logger(log::LevelFilter::Warn);

    // Disable macOS VPIO (VoiceProcessingIO) for cubeb audio streams.
    // VPIO creates an aggregate device that hangs with BlackHole virtual audio
    // drivers.  Voice processing (AEC/AGC/NS) is unnecessary for virtual audio.
    // Safety: called before any threads are spawned, single-threaded at this point.
    unsafe { std::env::set_var("RINGRTC_NO_VOICE_PROCESSING", "1") };

    let audio_config = AudioConfig::default();
    let mut pcf = PeerConnectionFactory::new(&audio_config, false, "", None)?;

    // Wait for cubeb to enumerate the virtual devices
    loop {
        std::thread::sleep(Duration::from_millis(100));
        if pcf
            .get_audio_playout_devices()
            .is_ok_and(|d| !d.is_empty())
            && pcf
                .get_audio_recording_devices()
                .is_ok_and(|d| !d.is_empty())
        {
            break;
        }
    }

    // Select virtual devices by name.
    //
    // We can't use set_audio_*_device_by_id() because the ADM matches on the
    // cubeb unique_id (e.g. "signal_input2ch_UID"), not the friendly name we
    // know ("signal_input").  Instead, enumerate and find the index by name.
    let input_name = virtual_audio.input_source();
    let recording_devices = pcf.get_audio_recording_devices()?;
    let recording_index = recording_devices
        .iter()
        .position(|d| d.name == input_name)
        .ok_or_else(|| anyhow::anyhow!("recording device '{}' not found", input_name))?
        as u16;
    pcf.set_audio_recording_device(recording_index)?;
    info!("Selected recording device: index={}, name={}", recording_index, input_name);

    let output_name = virtual_audio.output_sink();
    let playout_devices = pcf.get_audio_playout_devices()?;
    let playout_index = playout_devices
        .iter()
        .position(|d| d.name == output_name)
        .ok_or_else(|| anyhow::anyhow!("playout device '{}' not found", output_name))?
        as u16;
    pcf.set_audio_playout_device(playout_index)?;
    info!("Selected playout device: index={}, name={}", playout_index, output_name);

    // Create platform with our trait implementations
    let signaling_sender = Box::new(TunnelSignalingSender {
        event_sender: event_sender.clone(),
    });
    let state_handler = Box::new(TunnelStateHandler {
        event_sender: event_sender.clone(),
    });
    let group_handler = Box::new(TunnelGroupHandler);

    let platform = NativePlatform::new(
        pcf.clone(),
        signaling_sender,
        true, // should_assume_messages_sent
        state_handler,
        group_handler,
    );

    let http_client = http::DelegatingClient::new(NullHttpClient);
    let mut call_manager = CallManager::new(platform, http_client)?;

    // Peer ID is set by the first createOutgoingCall or receivedOffer message.
    let mut active_peer_id = PeerId::from("remote");
    let call_id = CallId::from(config.call_id);
    let local_device_id = config.local_device_id as DeviceId;

    info!("CallManager initialized, entering event loop");

    // Spawn a thread to forward platform events to control channel
    let control_writer = control.writer.clone();
    std::thread::spawn(move || {
        for event in event_receiver {
            control_writer.send_event(&event);
        }
    });

    // Main event loop: process control messages
    loop {
        let msg = match control.msg_receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(msg) => msg,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                info!("Control channel disconnected, exiting");
                break;
            }
        };

        match msg {
            ControlMessage::CreateOutgoingCall {
                call_id: cid,
                peer_id: pid,
            } => {
                let call_id = CallId::from(cid);
                let peer_id = PeerId::from(pid.as_str());
                active_peer_id = peer_id.clone();
                info!("Creating outgoing call: call_id={}, peer_id={}", cid, pid);
                if let Err(e) = call_manager.create_outgoing_call(
                    peer_id,
                    call_id,
                    CallMediaType::Audio,
                    local_device_id,
                ) {
                    error!("Failed to create outgoing call: {}", e);
                    control.writer.send_line(&format!(
                        r#"{{"type":"error","message":"failed to create outgoing call: {}"}}"#,
                        e
                    ));
                }
            }
            ControlMessage::Proceed {
                call_id: cid,
                ice_servers,
                hide_ip,
            } => {
                let call_id = CallId::from(cid);
                info!("Proceeding with call: call_id={}", cid);

                let ice_server_list: Vec<IceServer> = ice_servers
                    .iter()
                    .map(|s| IceServer::new(
                        s.username.clone(),
                        s.password.clone(),
                        String::new(),
                        s.urls.clone(),
                    ))
                    .collect();

                let outgoing_audio_track = match pcf.create_outgoing_audio_track() {
                    Ok(t) => t,
                    Err(e) => {
                        error!("Failed to create audio track: {}", e);
                        continue;
                    }
                };
                let outgoing_video_source = match pcf.create_outgoing_video_source() {
                    Ok(s) => s,
                    Err(e) => {
                        error!("Failed to create video source: {}", e);
                        continue;
                    }
                };
                let outgoing_video_track =
                    match pcf.create_outgoing_video_track(&outgoing_video_source) {
                        Ok(t) => t,
                        Err(e) => {
                            error!("Failed to create video track: {}", e);
                            continue;
                        }
                    };

                let call_context = NativeCallContext::new(
                    hide_ip,
                    ice_server_list,
                    outgoing_audio_track,
                    outgoing_video_track,
                    Box::new(NullVideoSink),
                );

                let call_config = CallConfig {
                    data_mode: DataMode::Low,
                    ..Default::default()
                };

                if let Err(e) =
                    call_manager.proceed(call_id, call_context, call_config, None)
                {
                    error!("Failed to proceed: {}", e);
                    control.writer.send_line(&format!(
                        r#"{{"type":"error","message":"failed to proceed: {}"}}"#,
                        e
                    ));
                }
            }
            ControlMessage::ReceivedOffer {
                call_id: cid,
                peer_id: pid,
                sender_device_id,
                opaque,
                age_ms,
                sender_identity_key,
                receiver_identity_key,
            } => {
                let call_id = CallId::from(cid);
                let peer_id = PeerId::from(pid.as_str());
                active_peer_id = peer_id.clone();
                info!(
                    "Received offer: call_id={}, peer_id={}, sender_device={}",
                    cid, pid, sender_device_id
                );

                let offer = match signaling::Offer::new(CallMediaType::Audio, opaque) {
                    Ok(o) => o,
                    Err(e) => {
                        error!("Failed to parse offer: {}", e);
                        continue;
                    }
                };

                let received = signaling::ReceivedOffer {
                    offer,
                    age: Duration::from_millis(age_ms),
                    sender_device_id: sender_device_id as DeviceId,
                    receiver_device_id: local_device_id,
                    sender_identity_key,
                    receiver_identity_key,
                };

                if let Err(e) = call_manager.received_offer(
                    peer_id,
                    call_id,
                    received,
                ) {
                    error!("Failed to process received offer: {}", e);
                }
            }
            ControlMessage::ReceivedAnswer {
                opaque,
                sender_device_id,
                sender_identity_key,
                receiver_identity_key,
            } => {
                info!("Received answer from device {}", sender_device_id);

                let answer = match signaling::Answer::new(opaque) {
                    Ok(a) => a,
                    Err(e) => {
                        error!("Failed to parse answer: {}", e);
                        continue;
                    }
                };

                let received = signaling::ReceivedAnswer {
                    answer,
                    sender_device_id: sender_device_id as DeviceId,
                    sender_identity_key,
                    receiver_identity_key,
                };

                if let Err(e) = call_manager.received_answer(
                    active_peer_id.clone(),
                    call_id,
                    received,
                ) {
                    error!("Failed to process received answer: {}", e);
                }
            }
            ControlMessage::ReceivedIce { candidates } => {
                debug!("Received {} ICE candidates", candidates.len());

                let ice_candidates: Vec<signaling::IceCandidate> = candidates
                    .into_iter()
                    .map(signaling::IceCandidate::new)
                    .collect();

                let received = signaling::ReceivedIce {
                    ice: signaling::Ice {
                        candidates: ice_candidates,
                    },
                    sender_device_id: 1 as DeviceId,
                };

                if let Err(e) = call_manager.received_ice(
                    active_peer_id.clone(),
                    call_id,
                    received,
                ) {
                    error!("Failed to process received ICE: {}", e);
                }
            }
            ControlMessage::Accept => {
                info!("Accepting call");
                if let Err(e) = call_manager.accept_call(call_id) {
                    error!("Failed to accept call: {}", e);
                }
            }
            ControlMessage::Hangup => {
                info!("Hanging up");
                if let Err(e) = call_manager.hangup() {
                    error!("Failed to hangup: {}", e);
                }
                // Give time for hangup to be sent
                std::thread::sleep(Duration::from_millis(500));
                break;
            }
        }
    }

    info!("signal-call-tunnel exiting");
    Ok(())
}
