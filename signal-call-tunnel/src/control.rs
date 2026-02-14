use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::mpsc;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use log::{error, info, warn};
use serde_json::Value;
use subtle::ConstantTimeEq;

use crate::platform::PlatformEvent;

/// Messages parsed from the parent process.
#[derive(Debug)]
pub enum ControlMessage {
    Auth { token: String },
    CreateOutgoingCall { call_id: u64, peer_id: String },
    Proceed { call_id: u64, ice_servers: Vec<IceServerConfig>, hide_ip: bool },
    ReceivedOffer {
        call_id: u64,
        peer_id: String,
        sender_device_id: u32,
        opaque: Vec<u8>,
        age_ms: u64,
        sender_identity_key: Vec<u8>,
        receiver_identity_key: Vec<u8>,
    },
    ReceivedAnswer {
        opaque: Vec<u8>,
        sender_device_id: u32,
        sender_identity_key: Vec<u8>,
        receiver_identity_key: Vec<u8>,
    },
    ReceivedIce { candidates: Vec<Vec<u8>> },
    Accept,
    Hangup,
}

#[derive(Debug, Clone)]
pub struct IceServerConfig {
    pub username: String,
    pub password: String,
    pub urls: Vec<String>,
}

/// Parse a JSON line into a ControlMessage.
pub fn parse_message(line: &str) -> Result<ControlMessage> {
    let v: Value = serde_json::from_str(line).context("invalid JSON")?;
    let msg_type = v["type"].as_str().unwrap_or("");

    match msg_type {
        "auth" => Ok(ControlMessage::Auth {
            token: v["token"].as_str().unwrap_or("").to_string(),
        }),
        "createOutgoingCall" => Ok(ControlMessage::CreateOutgoingCall {
            call_id: v["callId"].as_u64().unwrap_or(0),
            peer_id: v["peerId"].as_str().unwrap_or("").to_string(),
        }),
        "proceed" => {
            let ice_servers = if let Some(servers) = v["iceServers"].as_array() {
                servers
                    .iter()
                    .map(|s| IceServerConfig {
                        username: s["username"].as_str().unwrap_or("").to_string(),
                        password: s["password"].as_str().unwrap_or("").to_string(),
                        urls: s["urls"]
                            .as_array()
                            .map(|urls| {
                                urls.iter()
                                    .filter_map(|u| u.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                    .collect()
            } else {
                Vec::new()
            };
            Ok(ControlMessage::Proceed {
                call_id: v["callId"].as_u64().unwrap_or(0),
                ice_servers,
                hide_ip: v["hideIp"].as_bool().unwrap_or(false),
            })
        }
        "receivedOffer" => Ok(ControlMessage::ReceivedOffer {
            call_id: v["callId"].as_u64().unwrap_or(0),
            peer_id: v["peerId"].as_str().unwrap_or("remote").to_string(),
            sender_device_id: v["senderDeviceId"].as_u64().unwrap_or(1) as u32,
            opaque: BASE64
                .decode(v["opaque"].as_str().unwrap_or(""))
                .unwrap_or_default(),
            age_ms: v["age"].as_u64().unwrap_or(0),
            sender_identity_key: BASE64
                .decode(v["senderIdentityKey"].as_str().unwrap_or(""))
                .unwrap_or_default(),
            receiver_identity_key: BASE64
                .decode(v["receiverIdentityKey"].as_str().unwrap_or(""))
                .unwrap_or_default(),
        }),
        "receivedAnswer" => Ok(ControlMessage::ReceivedAnswer {
            opaque: BASE64
                .decode(v["opaque"].as_str().unwrap_or(""))
                .unwrap_or_default(),
            sender_device_id: v["senderDeviceId"].as_u64().unwrap_or(1) as u32,
            sender_identity_key: BASE64
                .decode(v["senderIdentityKey"].as_str().unwrap_or(""))
                .unwrap_or_default(),
            receiver_identity_key: BASE64
                .decode(v["receiverIdentityKey"].as_str().unwrap_or(""))
                .unwrap_or_default(),
        }),
        "receivedIce" => {
            let candidates = if let Some(arr) = v["candidates"].as_array() {
                arr.iter()
                    .filter_map(|c| {
                        let b64 = c.as_str()?;
                        BASE64.decode(b64).ok()
                    })
                    .collect()
            } else {
                Vec::new()
            };
            Ok(ControlMessage::ReceivedIce { candidates })
        }
        "accept" => Ok(ControlMessage::Accept),
        "hangup" => Ok(ControlMessage::Hangup),
        _ => bail!("unknown message type: {}", msg_type),
    }
}

/// Validate the auth token using constant-time comparison.
pub fn validate_token(received: &str, expected: &str) -> bool {
    let received_bytes = received.as_bytes();
    let expected_bytes = expected.as_bytes();
    if received_bytes.len() != expected_bytes.len() {
        return false;
    }
    received_bytes.ct_eq(expected_bytes).into()
}

/// Runs the control channel server. Binds a Unix socket, accepts one connection,
/// validates auth, then reads messages and sends events.
///
/// Returns a channel receiver for incoming control messages and a writer for
/// sending events to the parent.
pub struct ControlChannel {
    pub msg_receiver: mpsc::Receiver<ControlMessage>,
    pub writer: ControlWriter,
}

#[derive(Clone)]
pub struct ControlWriter {
    sender: mpsc::Sender<String>,
}

impl ControlWriter {
    pub fn send_line(&self, line: &str) {
        if let Err(e) = self.sender.send(line.to_string()) {
            error!("Failed to send to control writer: {}", e);
        }
    }

    pub fn send_event(&self, event: &PlatformEvent) {
        self.send_line(&event.to_json());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_message tests ---

    #[test]
    fn parse_auth() {
        let msg = parse_message(r#"{"type":"auth","token":"secret123"}"#).unwrap();
        match msg {
            ControlMessage::Auth { token } => assert_eq!(token, "secret123"),
            _ => panic!("expected Auth, got {:?}", msg),
        }
    }

    #[test]
    fn parse_create_outgoing_call() {
        let msg = parse_message(
            r#"{"type":"createOutgoingCall","callId":42,"peerId":"abc-def"}"#,
        )
        .unwrap();
        match msg {
            ControlMessage::CreateOutgoingCall { call_id, peer_id } => {
                assert_eq!(call_id, 42);
                assert_eq!(peer_id, "abc-def");
            }
            _ => panic!("expected CreateOutgoingCall, got {:?}", msg),
        }
    }

    #[test]
    fn parse_proceed_with_ice_servers() {
        let json = r#"{
            "type": "proceed",
            "callId": 99,
            "hideIp": true,
            "iceServers": [
                {
                    "username": "user1",
                    "password": "pass1",
                    "urls": ["turn:example.com:3478", "stun:example.com:3478"]
                },
                {
                    "username": "user2",
                    "password": "pass2",
                    "urls": ["turn:other.com:443"]
                }
            ]
        }"#;
        let msg = parse_message(json).unwrap();
        match msg {
            ControlMessage::Proceed {
                call_id,
                ice_servers,
                hide_ip,
            } => {
                assert_eq!(call_id, 99);
                assert!(hide_ip);
                assert_eq!(ice_servers.len(), 2);
                assert_eq!(ice_servers[0].username, "user1");
                assert_eq!(ice_servers[0].password, "pass1");
                assert_eq!(ice_servers[0].urls.len(), 2);
                assert_eq!(ice_servers[0].urls[0], "turn:example.com:3478");
                assert_eq!(ice_servers[1].username, "user2");
                assert_eq!(ice_servers[1].urls.len(), 1);
            }
            _ => panic!("expected Proceed, got {:?}", msg),
        }
    }

    #[test]
    fn parse_proceed_no_ice_servers() {
        let msg = parse_message(r#"{"type":"proceed","callId":1}"#).unwrap();
        match msg {
            ControlMessage::Proceed {
                call_id,
                ice_servers,
                hide_ip,
            } => {
                assert_eq!(call_id, 1);
                assert!(!hide_ip);
                assert!(ice_servers.is_empty());
            }
            _ => panic!("expected Proceed, got {:?}", msg),
        }
    }

    #[test]
    fn parse_received_offer() {
        // "aGVsbG8=" is base64 for "hello"
        let json = r#"{
            "type": "receivedOffer",
            "callId": 100,
            "peerId": "0b949a17-dc53-41b1-9ebc-dea99cb93920",
            "senderDeviceId": 3,
            "opaque": "aGVsbG8=",
            "age": 500,
            "senderIdentityKey": "AQID",
            "receiverIdentityKey": "BAUG"
        }"#;
        let msg = parse_message(json).unwrap();
        match msg {
            ControlMessage::ReceivedOffer {
                call_id,
                peer_id,
                sender_device_id,
                opaque,
                age_ms,
                sender_identity_key,
                receiver_identity_key,
            } => {
                assert_eq!(call_id, 100);
                assert_eq!(peer_id, "0b949a17-dc53-41b1-9ebc-dea99cb93920");
                assert_eq!(sender_device_id, 3);
                assert_eq!(opaque, b"hello");
                assert_eq!(age_ms, 500);
                assert_eq!(sender_identity_key, vec![1, 2, 3]);
                assert_eq!(receiver_identity_key, vec![4, 5, 6]);
            }
            _ => panic!("expected ReceivedOffer, got {:?}", msg),
        }
    }

    #[test]
    fn parse_received_answer() {
        let json = r#"{
            "type": "receivedAnswer",
            "opaque": "AQID",
            "senderDeviceId": 2,
            "senderIdentityKey": "BAUG",
            "receiverIdentityKey": "BwgJ"
        }"#;
        let msg = parse_message(json).unwrap();
        match msg {
            ControlMessage::ReceivedAnswer {
                opaque,
                sender_device_id,
                sender_identity_key,
                receiver_identity_key,
            } => {
                assert_eq!(opaque, vec![1, 2, 3]);
                assert_eq!(sender_device_id, 2);
                assert_eq!(sender_identity_key, vec![4, 5, 6]);
                assert_eq!(receiver_identity_key, vec![7, 8, 9]);
            }
            _ => panic!("expected ReceivedAnswer, got {:?}", msg),
        }
    }

    #[test]
    fn parse_received_ice() {
        // Two base64-encoded candidates
        let json = r#"{"type":"receivedIce","candidates":["AQID","BAUG"]}"#;
        let msg = parse_message(json).unwrap();
        match msg {
            ControlMessage::ReceivedIce { candidates } => {
                assert_eq!(candidates.len(), 2);
                assert_eq!(candidates[0], vec![1, 2, 3]);
                assert_eq!(candidates[1], vec![4, 5, 6]);
            }
            _ => panic!("expected ReceivedIce, got {:?}", msg),
        }
    }

    #[test]
    fn parse_received_ice_empty() {
        let msg = parse_message(r#"{"type":"receivedIce","candidates":[]}"#).unwrap();
        match msg {
            ControlMessage::ReceivedIce { candidates } => {
                assert!(candidates.is_empty());
            }
            _ => panic!("expected ReceivedIce, got {:?}", msg),
        }
    }

    #[test]
    fn parse_accept() {
        let msg = parse_message(r#"{"type":"accept"}"#).unwrap();
        assert!(matches!(msg, ControlMessage::Accept));
    }

    #[test]
    fn parse_hangup() {
        let msg = parse_message(r#"{"type":"hangup"}"#).unwrap();
        assert!(matches!(msg, ControlMessage::Hangup));
    }

    #[test]
    fn parse_unknown_type_fails() {
        let result = parse_message(r#"{"type":"foobar"}"#);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown message type"));
    }

    #[test]
    fn parse_invalid_json_fails() {
        let result = parse_message("not json at all");
        assert!(result.is_err());
    }

    #[test]
    fn parse_missing_type_fails() {
        let result = parse_message(r#"{"callId":1}"#);
        assert!(result.is_err());
    }

    // --- validate_token tests ---

    #[test]
    fn validate_token_matching() {
        assert!(validate_token("my-secret-token", "my-secret-token"));
    }

    #[test]
    fn validate_token_mismatch() {
        assert!(!validate_token("wrong-token", "my-secret-token"));
    }

    #[test]
    fn validate_token_different_lengths() {
        assert!(!validate_token("short", "a-much-longer-token"));
    }

    #[test]
    fn validate_token_empty() {
        assert!(validate_token("", ""));
    }

    #[test]
    fn validate_token_one_empty() {
        assert!(!validate_token("", "notempty"));
        assert!(!validate_token("notempty", ""));
    }
}

pub fn start_control_channel(
    control_socket_path: &str,
    expected_token: &str,
    input_device_name: &str,
    output_device_name: &str,
) -> Result<ControlChannel> {
    // Remove stale socket file
    let _ = std::fs::remove_file(control_socket_path);

    let listener = UnixListener::bind(control_socket_path)
        .with_context(|| format!("failed to bind control socket at {}", control_socket_path))?;
    info!("Control channel listening on {}", control_socket_path);

    let (msg_sender, msg_receiver) = mpsc::channel::<ControlMessage>();
    let (write_sender, write_receiver) = mpsc::channel::<String>();
    let writer = ControlWriter {
        sender: write_sender,
    };

    // Send ready message immediately (parent can connect after this)
    let ready_msg = format!(
        r#"{{"type":"ready","inputDeviceName":"{}","outputDeviceName":"{}"}}"#,
        input_device_name, output_device_name
    );

    let expected_token = expected_token.to_string();

    // Spawn reader thread
    std::thread::spawn(move || {
        // Accept one connection
        let (stream, _) = match listener.accept() {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to accept control connection: {}", e);
                return;
            }
        };
        info!("Control channel: parent connected");

        let mut writer_stream = match stream.try_clone() {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to clone control stream: {}", e);
                return;
            }
        };

        // Spawn writer thread
        std::thread::spawn(move || {
            for line in write_receiver {
                if let Err(e) = writeln!(writer_stream, "{}", line) {
                    error!("Failed to write to control channel: {}", e);
                    break;
                }
                if let Err(e) = writer_stream.flush() {
                    error!("Failed to flush control channel: {}", e);
                    break;
                }
            }
        });

        let reader = BufReader::new(stream);
        let mut authenticated = false;

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    info!("Control channel read ended: {}", e);
                    break;
                }
            };

            if line.trim().is_empty() {
                continue;
            }

            let msg = match parse_message(&line) {
                Ok(m) => m,
                Err(e) => {
                    warn!("Failed to parse control message: {} (line: {})", e, line);
                    continue;
                }
            };

            // First message must be auth
            if !authenticated {
                if let ControlMessage::Auth { ref token } = msg {
                    if validate_token(token, &expected_token) {
                        authenticated = true;
                        info!("Control channel: authenticated");
                        continue;
                    } else {
                        error!("Control channel: auth failed");
                        break;
                    }
                } else {
                    error!("Control channel: first message must be auth");
                    break;
                }
            }

            if let Err(e) = msg_sender.send(msg) {
                info!("Control message receiver dropped: {}", e);
                break;
            }
        }
    });

    // Send the ready message through the writer channel
    // (it will be sent once the writer thread starts)
    writer.send_line(&ready_msg);

    Ok(ControlChannel {
        msg_receiver,
        writer,
    })
}
