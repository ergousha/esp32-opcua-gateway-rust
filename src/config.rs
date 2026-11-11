//! Compile-time configuration: none, except during the transition.
//!
//! Everything about the cloud link comes from `platform.toml` (public) and NVS
//! `platform` (written at the bench by `ergo commission`): images carry no
//! secrets. Everything about the OPC UA session arrives through the `opcua`
//! device shadow.
//!
//! TRANSITIONAL: gateways in the field have their fallback Wi-Fi compiled in
//! from `cfg.toml` and nothing in NVS. This build, delivered only to the
//! private firmware bucket (never a GitHub Release), copies it into NVS
//! `platform` on first boot (`Platform::seed_wifi`). The next build drops
//! these two fields and reads Wi-Fi from NVS only.
#[toml_cfg::toml_config]
pub struct Config {
    #[default("")]
    pub wifi_ssid: &'static str,
    #[default("")]
    pub wifi_psk: &'static str,
}
