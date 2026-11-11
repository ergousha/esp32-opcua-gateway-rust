//! WiFi fallback: if wired Ethernet link/DHCP fails, device connects via WiFi.
//! ESP32-S3's internal WiFi is used; netif is attached to lwIP, so the same
//! MQTT/TLS stack runs.
//!
//! The credentials, first found wins:
//!
//! 1. NVS `platform` (`ergo commission --wifi-ssid` writes them at the bench);
//! 2. the station configuration ESP-IDF itself keeps in NVS
//!    (`nvs.net80211`): every gateway that ran an image with WiFi compiled in
//!    has it there, so moving to images without secrets needs no bench visit.
//!    It is copied into NVS `platform` once it is found;
//! 3. none: the unit waits for the bench console (a restart loop would cut
//!    `ergo commission` off).

use anyhow::{anyhow, Context, Result};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi};

/// Association attempts before giving up (see `start`).
const CONNECT_ATTEMPTS: u32 = 3;

/// Wrapper keeping the connected WiFi alive (if dropped, connection closes).
pub struct Wifi<'d> {
    _wifi: BlockingWifi<EspWifi<'d>>,
}

pub fn start<'d>(
    modem: Modem<'d>,
    sysloop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
    platform: &device_sdk::Platform,
) -> Result<Wifi<'d>> {
    let esp_wifi = EspWifi::new(modem, sysloop.clone(), Some(nvs)).context("EspWifi::new")?;
    let mut wifi = BlockingWifi::wrap(esp_wifi, sysloop).context("BlockingWifi::wrap")?;

    let (ssid, psk) = match platform.wifi() {
        Some(credentials) => credentials,
        None => match stored(&wifi) {
            Some((ssid, psk)) => {
                log::info!("WiFi: using the station configuration ESP-IDF kept in NVS");
                if let Err(e) = platform.seed_wifi(&ssid, &psk) {
                    log::warn!("could not copy it to NVS platform: {e:#}");
                }
                (ssid, psk)
            }
            None => {
                log::warn!("no WiFi credentials anywhere; waiting for WiFi from the bench console");
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    if let Some(credentials) = platform.wifi() {
                        break credentials;
                    }
                }
            }
        },
    };
    let (ssid, psk) = (ssid.as_str(), psk.as_str());

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
    // A few attempts in place before the caller restarts. A restart of an
    // image that has not marked itself valid yet is a rollback, and the
    // first association after a software reset times out now and then
    // (seen on the canary: an OTA update rolled back for it). Few, because a
    // failed attempt once left ~11 kB less heap.
    let mut attempt = 1;
    loop {
        match wifi.connect() {
            Ok(()) => break,
            Err(e) if attempt < CONNECT_ATTEMPTS => {
                log::warn!(
                    "WiFi connect attempt {attempt} of {CONNECT_ATTEMPTS} failed: {e}; retrying"
                );
                let _ = wifi.disconnect();
                std::thread::sleep(std::time::Duration::from_secs(3));
                attempt += 1;
            }
            Err(e) => return Err(e).context("wifi connect (SSID/password?)"),
        }
    }
    wifi.wait_netif_up().context("wifi netif did not come up")?;

    let ip = wifi.wifi().sta_netif().get_ip_info()?;
    log::info!("WiFi ready. IP: {ip:?}");

    Ok(Wifi { _wifi: wifi })
}

/// The station SSID and password ESP-IDF loaded from its own NVS storage.
fn stored(wifi: &BlockingWifi<EspWifi<'_>>) -> Option<(String, String)> {
    let client = match wifi.get_configuration().ok()? {
        Configuration::Client(c) | Configuration::Mixed(c, _) => c,
        _ => return None,
    };
    (!client.ssid.is_empty()).then(|| (client.ssid.to_string(), client.password.to_string()))
}
