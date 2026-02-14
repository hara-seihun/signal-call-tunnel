use std::collections::{HashMap, HashSet};
use std::sync::mpsc;

use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use log::{debug, error, info, warn};

use ringrtc::common::{CallId, CallMediaType, DeviceId};
use ringrtc::core::{group_call, signaling};
use ringrtc::lite::sfu::UserId;
use ringrtc::native::{
    CallState, CallStateHandler, GroupUpdate, GroupUpdateHandler, SignalingSender,
};
use ringrtc::webrtc::peer_connection::AudioLevel;
use ringrtc::webrtc::peer_connection_observer::NetworkRoute;

/// Events sent from the platform callbacks to the control channel writer.
#[derive(Debug)]
pub enum PlatformEvent {
    /// A signaling message to send to the parent process.
    SendSignaling {
        call_id: CallId,
        message: SignalingEvent,
    },
    /// A call state change.
    StateChange {
        state: String,
        reason: Option<String>,
    },
}

#[derive(Debug)]
pub enum SignalingEvent {
    SendOffer {
        opaque: Vec<u8>,
        call_media_type: CallMediaType,
    },
    SendAnswer {
        opaque: Vec<u8>,
    },
    SendIce {
        candidates: Vec<Vec<u8>>,
    },
    SendHangup {
        hangup_type: String,
    },
    SendBusy,
}

impl PlatformEvent {
    pub fn to_json(&self) -> String {
        match self {
            PlatformEvent::SendSignaling { call_id, message } => match message {
                SignalingEvent::SendOffer {
                    opaque,
                    call_media_type,
                } => {
                    let media_type = match call_media_type {
                        CallMediaType::Audio => "audio",
                        CallMediaType::Video => "video",
                    };
                    format!(
                        r#"{{"type":"sendOffer","callId":{},"opaque":"{}","callMediaType":"{}"}}"#,
                        u64::from(*call_id),
                        BASE64.encode(opaque),
                        media_type,
                    )
                }
                SignalingEvent::SendAnswer { opaque } => {
                    format!(
                        r#"{{"type":"sendAnswer","callId":{},"opaque":"{}"}}"#,
                        u64::from(*call_id),
                        BASE64.encode(opaque),
                    )
                }
                SignalingEvent::SendIce { candidates } => {
                    let candidates_json: Vec<String> = candidates
                        .iter()
                        .map(|c| format!(r#"{{"opaque":"{}"}}"#, BASE64.encode(c)))
                        .collect();
                    format!(
                        r#"{{"type":"sendIce","callId":{},"candidates":[{}]}}"#,
                        u64::from(*call_id),
                        candidates_json.join(","),
                    )
                }
                SignalingEvent::SendHangup { hangup_type } => {
                    format!(
                        r#"{{"type":"sendHangup","callId":{},"hangupType":"{}"}}"#,
                        u64::from(*call_id),
                        hangup_type,
                    )
                }
                SignalingEvent::SendBusy => {
                    format!(
                        r#"{{"type":"sendBusy","callId":{}}}"#,
                        u64::from(*call_id),
                    )
                }
            },
            PlatformEvent::StateChange { state, reason } => {
                if let Some(reason) = reason {
                    format!(
                        r#"{{"type":"stateChange","state":"{}","reason":"{}"}}"#,
                        state, reason,
                    )
                } else {
                    format!(r#"{{"type":"stateChange","state":"{}"}}"#, state)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn parse_event_json(event: &PlatformEvent) -> Value {
        serde_json::from_str(&event.to_json()).expect("event JSON should be valid")
    }

    #[test]
    fn send_offer_json() {
        let event = PlatformEvent::SendSignaling {
            call_id: CallId::from(42u64),
            message: SignalingEvent::SendOffer {
                opaque: vec![1, 2, 3],
                call_media_type: CallMediaType::Audio,
            },
        };
        let json = parse_event_json(&event);
        assert_eq!(json["type"], "sendOffer");
        assert_eq!(json["callId"], 42);
        assert_eq!(json["callMediaType"], "audio");
        // Verify opaque is valid base64 that decodes back
        let opaque_b64 = json["opaque"].as_str().unwrap();
        let decoded = BASE64.decode(opaque_b64).unwrap();
        assert_eq!(decoded, vec![1, 2, 3]);
    }

    #[test]
    fn send_offer_video_json() {
        let event = PlatformEvent::SendSignaling {
            call_id: CallId::from(1u64),
            message: SignalingEvent::SendOffer {
                opaque: vec![],
                call_media_type: CallMediaType::Video,
            },
        };
        let json = parse_event_json(&event);
        assert_eq!(json["callMediaType"], "video");
    }

    #[test]
    fn send_answer_json() {
        let event = PlatformEvent::SendSignaling {
            call_id: CallId::from(99u64),
            message: SignalingEvent::SendAnswer {
                opaque: vec![4, 5, 6],
            },
        };
        let json = parse_event_json(&event);
        assert_eq!(json["type"], "sendAnswer");
        assert_eq!(json["callId"], 99);
        let decoded = BASE64.decode(json["opaque"].as_str().unwrap()).unwrap();
        assert_eq!(decoded, vec![4, 5, 6]);
    }

    #[test]
    fn send_ice_json() {
        let event = PlatformEvent::SendSignaling {
            call_id: CallId::from(7u64),
            message: SignalingEvent::SendIce {
                candidates: vec![vec![10, 20], vec![30, 40]],
            },
        };
        let json = parse_event_json(&event);
        assert_eq!(json["type"], "sendIce");
        assert_eq!(json["callId"], 7);
        let candidates = json["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), 2);
        let c0 = BASE64
            .decode(candidates[0]["opaque"].as_str().unwrap())
            .unwrap();
        assert_eq!(c0, vec![10, 20]);
        let c1 = BASE64
            .decode(candidates[1]["opaque"].as_str().unwrap())
            .unwrap();
        assert_eq!(c1, vec![30, 40]);
    }

    #[test]
    fn send_ice_empty_candidates_json() {
        let event = PlatformEvent::SendSignaling {
            call_id: CallId::from(1u64),
            message: SignalingEvent::SendIce {
                candidates: vec![],
            },
        };
        let json = parse_event_json(&event);
        assert_eq!(json["candidates"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn send_hangup_json() {
        let event = PlatformEvent::SendSignaling {
            call_id: CallId::from(50u64),
            message: SignalingEvent::SendHangup {
                hangup_type: "normal".to_string(),
            },
        };
        let json = parse_event_json(&event);
        assert_eq!(json["type"], "sendHangup");
        assert_eq!(json["callId"], 50);
        assert_eq!(json["hangupType"], "normal");
    }

    #[test]
    fn send_busy_json() {
        let event = PlatformEvent::SendSignaling {
            call_id: CallId::from(8u64),
            message: SignalingEvent::SendBusy,
        };
        let json = parse_event_json(&event);
        assert_eq!(json["type"], "sendBusy");
        assert_eq!(json["callId"], 8);
    }

    #[test]
    fn state_change_with_reason_json() {
        let event = PlatformEvent::StateChange {
            state: "Ended".to_string(),
            reason: Some("Timeout".to_string()),
        };
        let json = parse_event_json(&event);
        assert_eq!(json["type"], "stateChange");
        assert_eq!(json["state"], "Ended");
        assert_eq!(json["reason"], "Timeout");
    }

    #[test]
    fn state_change_without_reason_json() {
        let event = PlatformEvent::StateChange {
            state: "Connected".to_string(),
            reason: None,
        };
        let json = parse_event_json(&event);
        assert_eq!(json["type"], "stateChange");
        assert_eq!(json["state"], "Connected");
        assert!(json.get("reason").is_none());
    }

    // --- Round-trip tests: platform event -> JSON -> parse as control message ---

    #[test]
    fn round_trip_offer() {
        let opaque = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let event = PlatformEvent::SendSignaling {
            call_id: CallId::from(123u64),
            message: SignalingEvent::SendOffer {
                opaque: opaque.clone(),
                call_media_type: CallMediaType::Audio,
            },
        };
        let json_str = event.to_json();
        // The parent would receive this JSON and could extract the opaque
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        let decoded = BASE64
            .decode(parsed["opaque"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, opaque);
    }

    #[test]
    fn round_trip_ice() {
        let candidates = vec![vec![1, 2, 3], vec![4, 5, 6, 7, 8]];
        let event = PlatformEvent::SendSignaling {
            call_id: CallId::from(456u64),
            message: SignalingEvent::SendIce {
                candidates: candidates.clone(),
            },
        };
        let json_str = event.to_json();
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        let arr = parsed["candidates"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        for (i, c) in arr.iter().enumerate() {
            let decoded = BASE64.decode(c["opaque"].as_str().unwrap()).unwrap();
            assert_eq!(decoded, candidates[i]);
        }
    }
}

/// Implements SignalingSender -- relays signaling messages to the control channel.
pub struct TunnelSignalingSender {
    pub event_sender: mpsc::Sender<PlatformEvent>,
}

impl SignalingSender for TunnelSignalingSender {
    fn send_signaling(
        &self,
        _recipient_id: &str,
        call_id: CallId,
        _receiver_device_id: Option<DeviceId>,
        message: signaling::Message,
    ) -> Result<()> {
        let event = match message {
            signaling::Message::Offer(offer) => SignalingEvent::SendOffer {
                opaque: offer.opaque,
                call_media_type: offer.call_media_type,
            },
            signaling::Message::Answer(answer) => SignalingEvent::SendAnswer {
                opaque: answer.opaque,
            },
            signaling::Message::Ice(ice) => SignalingEvent::SendIce {
                candidates: ice.candidates.into_iter().map(|c| c.opaque).collect(),
            },
            signaling::Message::Hangup(hangup) => {
                let (hangup_type, _device_id) = hangup.to_type_and_device_id();
                SignalingEvent::SendHangup {
                    hangup_type: format!("{:?}", hangup_type).to_lowercase(),
                }
            }
            signaling::Message::Busy => SignalingEvent::SendBusy,
        };

        if let Err(e) = self.event_sender.send(PlatformEvent::SendSignaling {
            call_id,
            message: event,
        }) {
            error!("Failed to send signaling event: {}", e);
        }
        Ok(())
    }

    fn send_call_message(
        &self,
        _recipient_id: UserId,
        _message: Vec<u8>,
        _urgency: group_call::SignalingMessageUrgency,
    ) -> Result<()> {
        // No-op for 1:1 calls
        Ok(())
    }

    fn send_call_message_to_group(
        &self,
        _group_id: group_call::GroupId,
        _message: Vec<u8>,
        _urgency: group_call::SignalingMessageUrgency,
        _recipients_override: HashSet<UserId>,
    ) -> Result<()> {
        // No-op for 1:1 calls
        Ok(())
    }

    fn send_call_message_to_adhoc_group(
        &self,
        _message: Vec<u8>,
        _urgency: group_call::SignalingMessageUrgency,
        _expiration: u64,
        _recipients_to_endorsements: HashMap<UserId, Vec<u8>>,
    ) -> Result<()> {
        // No-op for 1:1 calls
        Ok(())
    }
}

/// Implements CallStateHandler -- relays state changes to the control channel.
pub struct TunnelStateHandler {
    pub event_sender: mpsc::Sender<PlatformEvent>,
}

impl CallStateHandler for TunnelStateHandler {
    fn handle_call_state(
        &self,
        _remote_peer_id: &str,
        _call_id: CallId,
        call_state: CallState,
    ) -> Result<()> {
        let (state, reason) = match call_state {
            CallState::Incoming(media_type) => {
                (format!("Incoming({:?})", media_type), None)
            }
            CallState::Outgoing(media_type) => {
                (format!("Outgoing({:?})", media_type), None)
            }
            CallState::Ringing => ("Ringing".to_string(), None),
            CallState::Connected => ("Connected".to_string(), None),
            CallState::Connecting => ("Connecting".to_string(), None),
            CallState::Ended(reason, _summary) => {
                ("Ended".to_string(), Some(format!("{:?}", reason)))
            }
            CallState::Rejected(reason) => {
                ("Rejected".to_string(), Some(format!("{:?}", reason)))
            }
            CallState::Concluded => ("Concluded".to_string(), None),
        };

        info!("Call state: {} (reason: {:?})", state, reason);

        if let Err(e) = self
            .event_sender
            .send(PlatformEvent::StateChange { state, reason })
        {
            error!("Failed to send state change event: {}", e);
        }
        Ok(())
    }

    fn handle_remote_audio_state(
        &self,
        _remote_peer_id: &str,
        enabled: bool,
    ) -> Result<()> {
        debug!("Remote audio state: {}", enabled);
        Ok(())
    }

    fn handle_remote_video_state(
        &self,
        _remote_peer_id: &str,
        enabled: bool,
    ) -> Result<()> {
        debug!("Remote video state: {}", enabled);
        Ok(())
    }

    fn handle_remote_sharing_screen(
        &self,
        _remote_peer_id: &str,
        enabled: bool,
    ) -> Result<()> {
        debug!("Remote sharing screen: {}", enabled);
        Ok(())
    }

    fn handle_network_route(
        &self,
        _remote_peer_id: &str,
        network_route: NetworkRoute,
    ) -> Result<()> {
        info!("Network route: {:?}", network_route);
        Ok(())
    }

    fn handle_audio_levels(
        &self,
        _remote_peer_id: &str,
        _captured_level: AudioLevel,
        _received_level: AudioLevel,
    ) -> Result<()> {
        // Don't log -- too noisy
        Ok(())
    }

    fn handle_low_bandwidth_for_video(
        &self,
        _remote_peer_id: &str,
        recovered: bool,
    ) -> Result<()> {
        if recovered {
            info!("Low bandwidth for video: recovered");
        } else {
            warn!("Low bandwidth for video");
        }
        Ok(())
    }
}

/// Implements GroupUpdateHandler -- all no-ops for 1:1 calls.
pub struct TunnelGroupHandler;

impl GroupUpdateHandler for TunnelGroupHandler {
    fn handle_group_update(&self, _update: GroupUpdate) -> Result<()> {
        Ok(())
    }
}
