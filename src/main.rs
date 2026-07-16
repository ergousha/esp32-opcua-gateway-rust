//! ESP32-S3-ETH — AWS IoT Zero-Touch Provisioning (Fleet Provisioning by Claim).
//!
//! Boot flow:
//!   1. Bring up W5500 Ethernet with esp_eth (DHCP).
//!   2. Check if persistent device identity exists in NVS.
//!        - NO  -> Perform provisioning with claim certificate, write result to NVS.
//!        - YES -> Proceed directly with device identity.
//!   3. Connect to IoT Core with device identity and keep connection open.
//!
//! Pins (`../hardware/pins.png`, SPI2/FSPI):
//!   MOSI=GPIO11, MISO=GPIO12, SCLK=GPIO13, CS=GPIO14, INT=GPIO10, RST=GPIO9

mod config;
mod device_id;
mod eth;
mod mqtt_util;
mod ota;
mod provisioning;
mod telemetry;
mod wifi;

use anyhow::Result;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

/// Keeps the active network interface alive throughout main.
/// Even if fallback to WiFi occurs, the Ethernet handle is kept: if dropped, SpiDriver::drop
/// panics (see eth::start). Fields are only present to keep them alive.
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

    // --- 1) Network: wired Ethernet first, if no link/DHCP, fallback to WiFi (lives throughout main) ---
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
        log::info!("Network interface: Ethernet (W5500)");
        Net::Eth(eth_handle)
    } else {
        log::warn!("Ethernet unavailable; falling back to WiFi...");
        let wifi = wifi::start(peripherals.modem, sysloop, nvs_part.clone())?;
        log::info!("Network interface: WiFi");
        // eth_handle is kept ALIVE (panic prevention measure).
        Net::Wifi {
            eth: eth_handle,
            wifi,
        }
    };

    // --- 2) Persistent identity check -----------------------------------------
    let mut store = device_id::DeviceStore::new(nvs_part)?;

    let identity = if store.exists() {
        log::info!("Registered device identity found; skipping provisioning.");
        store.load()?
    } else {
        log::info!("No registered identity found; starting zero-touch provisioning.");
        let id = provisioning::run()?;
        store.save(&id)?;
        log::info!("Device identity saved to NVS.");
        id
    };

    // --- 3) Device connection (infinite loop) ---------------------------------
    telemetry::run(&identity)
}
