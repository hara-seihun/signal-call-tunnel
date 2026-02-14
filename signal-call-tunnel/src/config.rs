use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub call_id: u64,
    pub is_outgoing: bool,
    pub control_socket_path: String,
    pub control_token: String,
    pub local_device_id: u32,
    pub input_device_name: Option<String>,
    pub output_device_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_valid_config() {
        let json = r#"{
            "call_id": 12345678,
            "is_outgoing": true,
            "control_socket_path": "/tmp/sc-abc/ctrl.sock",
            "control_token": "dG9rZW4=",
            "local_device_id": 1
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.call_id, 12345678);
        assert!(config.is_outgoing);
        assert_eq!(config.control_socket_path, "/tmp/sc-abc/ctrl.sock");
        assert_eq!(config.control_token, "dG9rZW4=");
        assert_eq!(config.local_device_id, 1);
        assert!(config.input_device_name.is_none());
        assert!(config.output_device_name.is_none());
    }

    #[test]
    fn deserialize_with_device_names() {
        let json = r#"{
            "call_id": 99,
            "is_outgoing": false,
            "control_socket_path": "/tmp/ctrl.sock",
            "control_token": "tok",
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
            "control_socket_path": "/tmp/ctrl.sock",
            "control_token": "tok",
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
            "control_socket_path": "/tmp/ctrl.sock",
            "control_token": "tok",
            "local_device_id": 1,
            "extra_field": "ignored"
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.call_id, 1);
    }
}
