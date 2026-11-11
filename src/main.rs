//! ESP32-S3-ETH OPC UA gateway on the Ergousha IoT platform (iot-device-sdk).
//!
//! Boot flow:
//!   1. The platform SDK counts the boot, makes sure the device has its P-256
//!      key (generated while the radio is still off) and starts the bench
//!      console (`ergo commission`).
//!   2. Bring up W5500 Ethernet with esp_eth (DHCP); fall back to WiFi from
//!      NVS `platform`.
//!   3. The device's identity: the one in NVS, or enrolment with the claim
//!      written at the bench (a CSR for the soft key).
//!   4. Connect to AWS IoT and run the gateway (`telemetry::run`).
//!
//! Pins (`../hardware/pins.png`, SPI2/FSPI):
//!   MOSI=GPIO11, MISO=GPIO12, SCLK=GPIO13, CS=GPIO14, INT=GPIO10, RST=GPIO9

mod config;
mod eth;
mod mqtt_util;
mod settings_store;
mod shadow;
mod telemetry;
mod wifi;

use anyhow::Result;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

// `PLATFORM`: this product as platform.toml describes it (platform-build).
include!(concat!(env!("OUT_DIR"), "/platform.rs"));

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
    // The SDK keeps retrying AWS IoT with backoff; a new image that never
    // gets there is restarted after device_sdk::VERIFY_DEADLINE and rolled back.
    device_sdk::start_rollback_guard();

    let result = run();
    if let Err(e) = &result {
        // Returning ends the main task and parks the device until someone
        // resets it: no MQTT, so no Jobs either, and no way to deliver a fix.
        // A fresh OTA image parked unverified is never rolled back, either; on
        // Ethernet an image that could not reach AWS IoT did exactly that.
        // Restarting hands an unverified image back to the previous one,
        // which then reports the job FAILED, and gives a valid one another go
        // at whatever failed: most often reaching AWS IoT.
        if device_sdk::running_unverified() {
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

    // Before the network: a soft key generated now draws on the RNG's
    // radio-off entropy source.
    let platform = device_sdk::Platform::init(&PLATFORM, nvs_part.clone())?;
    // TRANSITIONAL (config.rs): the compiled-in fallback WiFi moves to NVS.
    match platform.seed_wifi(config::CONFIG.wifi_ssid, config::CONFIG.wifi_psk) {
        Ok(true) => log::info!("fallback WiFi moved to NVS"),
        Ok(false) => {}
        Err(e) => log::warn!("could not store the fallback WiFi in NVS: {e:#}"),
    }

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
        // A unit with no WiFi in NVS (a new board on the bench) waits for
        // `ergo commission` to write it over the console, which a restart
        // loop would keep cutting off. Once it is there, carry on.
        let mut credentials = platform.wifi();
        if credentials.is_none() {
            log::warn!("no Ethernet and no WiFi in NVS; waiting for WiFi from the bench console");
            while credentials.is_none() {
                std::thread::sleep(std::time::Duration::from_secs(5));
                credentials = platform.wifi();
            }
        }
        match wifi::start(peripherals.modem, sysloop, nvs_part.clone(), credentials) {
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

    // --- 2) Identity: stored, or enrolment with the bench-provisioned claim ---
    let identity = platform.identity()?;

    // --- 3) Device connection (infinite loop) ---------------------------------
    // The input buffer holds a whole retained tag bundle: esp-mqtt reports the
    // topic only on a message's first chunk.
    let session = platform.connect(
        &identity,
        &device_sdk::ConnectOptions {
            buffer_size: 12 * 1024,
        },
    )?;
    telemetry::run(session)
}
