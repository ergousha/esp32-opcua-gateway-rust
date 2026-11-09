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
mod job_store;
mod jobs;
mod mqtt_util;
mod ota;
mod provisioning;
mod settings_store;
mod shadow;
mod telemetry;
mod wifi;

use anyhow::Result;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

/// Stub socketpair symbol for ESP-IDF target (Unix Domain Sockets unavailable in ESP-IDF libc).
///
/// # Safety
///
/// Never dereferences `_sv` and touches no other state; it only reports
/// failure, so any arguments are sound. `unsafe` only because it is an
/// `extern "C"` symbol that C code calls with raw pointers.
#[no_mangle]
pub unsafe extern "C" fn socketpair(
    _domain: std::os::raw::c_int,
    _type: std::os::raw::c_int,
    _protocol: std::os::raw::c_int,
    _sv: *mut std::os::raw::c_int,
) -> std::os::raw::c_int {
    -1
}

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

    let result = run();
    if let Err(e) = &result {
        // Returning ends the main task and parks the device until someone
        // resets it: no MQTT, so no Jobs either, and no way to deliver a fix.
        // A fresh OTA image parked unverified is never rolled back, either; on
        // Ethernet an image that could not reach AWS IoT did exactly that.
        // Restarting hands an unverified image back to the previous one,
        // which then reports the job FAILED, and gives a valid one another go
        // at whatever failed: most often reaching AWS IoT.
        if ota::running_unverified() {
            log::error!("{e:#}; restarting so the bootloader restores the previous image");
            std::thread::sleep(std::time::Duration::from_secs(2));
        } else {
            log::error!("{e:#}; restarting in {} s", RESTART_AFTER_FAILURE.as_secs());
            std::thread::sleep(RESTART_AFTER_FAILURE);
        }
        esp_idf_svc::hal::reset::restart();
    }
    result
}

/// How long a valid image waits after `run` failed before it restarts. Long
/// enough that a failure that persists, such as AWS IoT being unreachable,
/// does not turn into a tight boot loop; each attempt already spends up to
/// 30 s waiting for the connection.
const RESTART_AFTER_FAILURE: std::time::Duration = std::time::Duration::from_secs(30);

fn run() -> Result<()> {
    // Claim the OPC UA type table before TLS and the OPC UA session take their
    // share of the heap. It is ~9 kB in one block, built lazily on the first
    // ExtensionObject decode; deferred, that decode lands when the heap is
    // nearly gone and the allocation aborts the process instead of failing
    // softly. Paid here, it is paid out of ~230 kB rather than out of nothing.
    gateway_opcua::preload_types();
    log::info!("OPC UA type table preloaded; free heap {}", unsafe {
        esp_idf_svc::sys::esp_get_free_heap_size()
    });

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
        match wifi::start(peripherals.modem, sysloop, nvs_part.clone()) {
            Ok(wifi) => {
                log::info!("Network interface: WiFi");
                Net::Wifi {
                    eth: eth_handle,
                    wifi,
                }
            }
            Err(e) => {
                // A restart retries Ethernet too; an OTA image not yet marked valid is rolled back.
                log::error!("WiFi start failed: {:?}; restarting in 10 s", e);
                std::thread::sleep(std::time::Duration::from_secs(10));
                esp_idf_svc::hal::reset::restart();
            }
        }
    };

    // --- 2) Persistent identity check -----------------------------------------
    let mut store = device_id::DeviceStore::new(nvs_part.clone())?;
    let job_store = job_store::JobStore::new(nvs_part)?;

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
    telemetry::run(&identity, job_store)
}
