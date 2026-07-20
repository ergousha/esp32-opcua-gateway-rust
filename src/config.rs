//! PoC configuration. In production, move these values to `toml-cfg`/NVS/eFuse.
//!
//! Network: wired Ethernet (DHCP) is tried first; if no link/DHCP, falls back to WiFi.
//! WiFi credentials are embedded at compile time from `cfg.toml` (in gitignore).

/// WiFi credentials — read from [esp32-opcua-gateway] table inside `cfg.toml`
/// at compile time via toml-cfg. Access: `config::CONFIG.wifi_ssid`.
#[toml_cfg::toml_config]
pub struct Config {
    #[default("")]
    pub wifi_ssid: &'static str,
    #[default("")]
    pub wifi_psk: &'static str,
    #[default("")]
    pub iot_endpoint: &'static str,
    #[default("")]
    pub provisioning_template: &'static str,
}

/// Shared secret matching the record in DynamoDB.
/// PoC: common to all devices. In production, it must be unique per device and
/// protected with ESP32-S3 DS peripheral.
pub const DEVICE_SECRET: &str = "change-me-shared-secret";

/// MQTT connection URL (mutual TLS, port 8883).
pub fn mqtt_url() -> String {
    format!("mqtts://{}:8883", CONFIG.iot_endpoint)
}
