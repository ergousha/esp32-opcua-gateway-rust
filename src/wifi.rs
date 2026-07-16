//! WiFi fallback: if wired Ethernet link/DHCP fails, device connects via WiFi.
//! Credentials come from `cfg.toml` (toml-cfg). ESP32-S3's internal WiFi
//! is used; netif is attached to lwIP, so the same MQTT/TLS stack runs.

use anyhow::{anyhow, bail, Context, Result};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi};

use crate::config;

/// Wrapper keeping the connected WiFi alive (if dropped, connection closes).
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
        bail!("WiFi SSID is empty (cfg.toml -> [esp32-opcua-gateway].wifi_ssid)");
    }

    let esp_wifi = EspWifi::new(modem, sysloop.clone(), Some(nvs)).context("EspWifi::new")?;
    let mut wifi = BlockingWifi::wrap(esp_wifi, sysloop).context("BlockingWifi::wrap")?;

    let auth_method = if psk.is_empty() {
        AuthMethod::None
    } else {
        AuthMethod::WPA2Personal
    };

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: ssid
            .try_into()
            .map_err(|_| anyhow!("SSID exceeds 32 characters"))?,
        password: psk
            .try_into()
            .map_err(|_| anyhow!("PSK exceeds 64 characters"))?,
        auth_method,
        ..Default::default()
    }))?;

    wifi.start().context("wifi start")?;
    log::info!("Connecting to WiFi: SSID={ssid}");
    wifi.connect().context("wifi connect (SSID/password?)")?;
    wifi.wait_netif_up().context("wifi netif did not come up")?;

    let ip = wifi.wifi().sta_netif().get_ip_info()?;
    log::info!("WiFi ready. IP: {ip:?}");

    Ok(Wifi { _wifi: wifi })
}
