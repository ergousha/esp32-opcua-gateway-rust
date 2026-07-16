//! PoC yapilandirmasi. Uretimde bu degerleri `toml-cfg`/NVS/eFuse'a tasiyin.
//!
//! Ag: once kablolu Ethernet (DHCP) denenir; link/DHCP yoksa WiFi'ye dusulur.
//! WiFi kimlik bilgileri `cfg.toml`'dan (gitignore'da) derleme aninda gomulur.

/// WiFi kimlik bilgileri — `cfg.toml` icindeki [esp32-opcua-gateway] tablosundan
/// toml-cfg ile derleme aninda okunur. Erisim: `config::CONFIG.wifi_ssid`.
#[toml_cfg::toml_config]
pub struct Config {
    #[default("")]
    pub wifi_ssid: &'static str,
    #[default("")]
    pub wifi_psk: &'static str,
}

/// AWS IoT ATS veri endpoint'i. `terraform output -raw iot_endpoint` ciktisi.
pub const MQTT_ENDPOINT: &str = "a2pw25e7ewpuwe-ats.iot.eu-central-1.amazonaws.com";

/// Fleet Provisioning template adi. Terraform'daki `provisioning_template_name`
/// ile AYNI olmali.
pub const PROVISIONING_TEMPLATE: &str = "esp32-s3-fleet-template";

/// DynamoDB'deki kayitla eslesen paylasilan secret.
/// PoC: tum cihazlarda ortak. Uretimde cihaz basina benzersiz olmali ve
/// ESP32-S3 DS peripheral ile korunmali.
pub const DEVICE_SECRET: &str = "change-me-shared-secret";



/// MQTT baglanti URL'i (mutual TLS, port 8883).
pub fn mqtt_url() -> String {
    format!("mqtts://{MQTT_ENDPOINT}:8883")
}
