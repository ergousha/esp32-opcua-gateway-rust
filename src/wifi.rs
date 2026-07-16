//! WiFi fallback: kablolu Ethernet link/DHCP vermezse cihaz WiFi ile baglanir.
//! Kimlik bilgileri `cfg.toml`'dan (toml-cfg) gelir. ESP32-S3'un dahili WiFi'si
//! kullanilir; netif lwIP'ye baglanir, boylece ayni MQTT/TLS yigini calisir.

use anyhow::{anyhow, bail, Context, Result};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{
    AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi,
};

use crate::config;

/// Baglanmis WiFi'yi canli tutan sarmalayici (drop olursa baglanti kapanir).
pub struct Wifi<'d> {
    _wifi: BlockingWifi<EspWifi<'d>>,
}

pub fn start<'d>(
    modem: Modem<'d>,
    sysloop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
) -> Result<Wifi<'d>> {
    let ssid = config::CONFIG.wifi_ssid;
    let psk = config::CONFIG.wifi_psk;
    if ssid.is_empty() {
        bail!("WiFi SSID bos (cfg.toml -> [esp32-opcua-gateway].wifi_ssid)");
    }

    let esp_wifi = EspWifi::new(modem, sysloop.clone(), Some(nvs)).context("EspWifi::new")?;
    let mut wifi = BlockingWifi::wrap(esp_wifi, sysloop).context("BlockingWifi::wrap")?;

    let auth_method = if psk.is_empty() {
        AuthMethod::None
    } else {
        AuthMethod::WPA2Personal
    };

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: ssid.try_into().map_err(|_| anyhow!("SSID 32 karakteri asiyor"))?,
        password: psk.try_into().map_err(|_| anyhow!("PSK 64 karakteri asiyor"))?,
        auth_method,
        ..Default::default()
    }))?;

    wifi.start().context("wifi start")?;
    log::info!("WiFi baglaniliyor: SSID={ssid}");
    wifi.connect().context("wifi connect (SSID/parola?)")?;
    wifi.wait_netif_up().context("wifi netif up olmadi")?;

    let ip = wifi.wifi().sta_netif().get_ip_info()?;
    log::info!("WiFi hazir. IP: {ip:?}");

    Ok(Wifi { _wifi: wifi })
}
