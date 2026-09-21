use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub call_id: u64,
    pub is_outgoing: bool,
    pub local_device_id: u32,
    pub input_device_name: Option<String>,
    pub output_device_name: Option<String>,
    /// Optional audio backend selector. When absent, the
    /// `SIGNAL_CALL_TUNNEL_AUDIO_MODE` environment variable is consulted, and
    /// the tunnel falls back to the virtual-device backend.
    #[serde(default)]
    pub audio_mode: Option<String>,
}

/// Which audio backend the tunnel drives for a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioMode {
    /// Real host audio via per-call virtual devices (the default).
    Device,
    /// Raw PCM exchanged with the parent over a Unix domain stream socket.
    Pipe,
}

/// Environment variable that selects the audio backend when the config does not.
pub const AUDIO_MODE_ENV: &str = "SIGNAL_CALL_TUNNEL_AUDIO_MODE";
/// Environment variable for the directory that holds pipe-mode sockets.
pub const SOCKET_DIR_ENV: &str = "SIGNAL_CALL_TUNNEL_SOCKET_DIR";

impl Config {
    /// Resolve the effective audio mode: the explicit `audio_mode` config field
    /// takes precedence, then [`AUDIO_MODE_ENV`], otherwise [`AudioMode::Device`].
    #[must_use]
    pub fn resolve_audio_mode(&self) -> AudioMode {
        let mode = self
            .audio_mode
            .clone()
            .or_else(|| std::env::var(AUDIO_MODE_ENV).ok())
            .unwrap_or_default();
        if mode.eq_ignore_ascii_case("pipe") {
            AudioMode::Pipe
        } else {
            AudioMode::Device
        }
    }

    /// Deterministic Unix socket path for pipe mode, under [`SOCKET_DIR_ENV`]
    /// (or the system temp dir) and keyed by call id.
    #[must_use]
    pub fn pipe_socket_path(&self) -> PathBuf {
        let dir = std::env::var_os(SOCKET_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        dir.join(format!("signal-call-{}.sock", self.call_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_valid_config() {
        let json = r#"{
            "call_id": 12345678,
            "is_outgoing": true,
            "local_device_id": 1
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.call_id, 12345678);
        assert!(config.is_outgoing);
        assert_eq!(config.local_device_id, 1);
        assert!(config.input_device_name.is_none());
        assert!(config.output_device_name.is_none());
    }

    #[test]
    fn deserialize_with_device_names() {
        let json = r#"{
            "call_id": 99,
            "is_outgoing": false,
            "local_device_id": 2,
            "input_device_name": "signal_input",
            "output_device_name": "signal_output"
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.call_id, 99);
        assert!(!config.is_outgoing);
        assert_eq!(config.local_device_id, 2);
        assert_eq!(config.input_device_name.as_deref(), Some("signal_input"));
        assert_eq!(config.output_device_name.as_deref(), Some("signal_output"));
    }

    #[test]
    fn deserialize_missing_field_fails() {
        let json = r#"{
            "call_id": 1,
            "is_outgoing": true
        }"#;
        let result: Result<Config, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_wrong_type_fails() {
        let json = r#"{
            "call_id": "not_a_number",
            "is_outgoing": true,
            "local_device_id": 1
        }"#;
        let result: Result<Config, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_extra_fields_ok() {
        let json = r#"{
            "call_id": 1,
            "is_outgoing": true,
            "local_device_id": 1,
            "extra_field": "ignored"
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.call_id, 1);
    }

    fn config_with_mode(mode: Option<&str>) -> Config {
        Config {
            call_id: 7,
            is_outgoing: false,
            local_device_id: 1,
            input_device_name: None,
            output_device_name: None,
            audio_mode: mode.map(str::to_string),
        }
    }

    #[test]
    fn audio_mode_defaults_to_none() {
        let json = r#"{
            "call_id": 1,
            "is_outgoing": true,
            "local_device_id": 1
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.audio_mode.is_none());
    }

    #[test]
    fn resolve_audio_mode_from_config_field() {
        // An explicit config field wins regardless of environment.
        assert_eq!(
            config_with_mode(Some("pipe")).resolve_audio_mode(),
            AudioMode::Pipe
        );
        assert_eq!(
            config_with_mode(Some("PIPE")).resolve_audio_mode(),
            AudioMode::Pipe
        );
        assert_eq!(
            config_with_mode(Some("device")).resolve_audio_mode(),
            AudioMode::Device
        );
        assert_eq!(
            config_with_mode(Some("anything-else")).resolve_audio_mode(),
            AudioMode::Device
        );
    }

    #[test]
    fn pipe_socket_path_is_keyed_by_call_id() {
        let path = config_with_mode(Some("pipe")).pipe_socket_path();
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("signal-call-7.sock")
        );
    }
}
