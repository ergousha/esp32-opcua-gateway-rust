//! ESP32-S3-ETH — AWS IoT Zero-Touch Provisioning (Fleet Provisioning by Claim).
//!
//! Boot akisi:
//!   1. W5500 Ethernet'i esp_eth ile ayaga kaldir (DHCP).
//!   2. NVS'de kalici cihaz kimligi var mi? bak.
//!        - YOK  -> claim sertifikasiyla provisioning yap, sonucu NVS'ye yaz.
//!        - VAR  -> dogrudan cihaz kimligiyle devam.
//!   3. Cihaz kimligiyle IoT Core'a baglanip baglantiyi acik tutar.
//!
//! Pinler (`../hardware/pins.png`, SPI2/FSPI):
//!   MOSI=GPIO11, MISO=GPIO12, SCLK=GPIO13, CS=GPIO14, INT=GPIO10, RST=GPIO9

mod config;
mod device_id;
mod eth;
mod mqtt_util;
mod provisioning;
mod telemetry;
mod wifi;

use anyhow::Result;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

/// Aktif ag arabirimini main boyunca canli tutar.
/// WiFi'ye dusulse bile eth handle'i tutulur: drop edilirse SpiDriver::drop
/// panikler (bkz. eth::start). Alanlar sadece canli tutmak icin var.
#[allow(clippy::large_enum_variant, dead_code)]
enum Net<'d> {
    Eth(eth::Eth<'d>),
    Wifi {
        eth: eth::Eth<'d>,
        wifi: wifi::Wifi<'d>,
    },
}

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs_part = EspDefaultNvsPartition::take()?;

    // --- 1) Ag: once kablolu Ethernet, link/DHCP yoksa WiFi (main boyunca yasar) ---
    let (eth_handle, eth_up) = eth::start(
        peripherals.spi2,
        peripherals.pins.gpio13, // SCLK
        peripherals.pins.gpio11, // MOSI
        peripherals.pins.gpio12, // MISO
        peripherals.pins.gpio14, // CS
        peripherals.pins.gpio10, // INT
        peripherals.pins.gpio9,  // RST
        sysloop.clone(),
    )?;

    let _net = if eth_up {
        log::info!("Ag arabirimi: Ethernet (W5500)");
        Net::Eth(eth_handle)
    } else {
        log::warn!("Ethernet yok; WiFi'ye geciliyor...");
        let wifi = wifi::start(peripherals.modem, sysloop, nvs_part.clone())?;
        log::info!("Ag arabirimi: WiFi");
        // eth_handle CANLI tutulur (drop panik onlemi).
        Net::Wifi {
            eth: eth_handle,
            wifi,
        }
    };

    // --- 2) Kalici kimlik var mi? -----------------------------------------
    let mut store = device_id::DeviceStore::new(nvs_part)?;

    let identity = if store.exists() {
        log::info!("Kayitli cihaz kimligi bulundu; provisioning atlaniyor.");
        store.load()?
    } else {
        log::info!("Kayitli kimlik yok; zero-touch provisioning baslatiliyor.");
        let id = provisioning::run()?;
        store.save(&id)?;
        log::info!("Cihaz kimligi NVS'ye kaydedildi.");
        id
    };

    // --- 3) Cihaz baglantisi (sonsuz dongu) --------------------------------------
    telemetry::run(&identity)
}
