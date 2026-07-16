//! Initializes W5500 Ethernet with ESP-IDF's internal esp_eth SPI driver
//! as a lwIP netif; blocks until IP is obtained via DHCP.
//!
//! Pins from `../hardware/pins.png` (SPI2/FSPI):
//!   MOSI=GPIO11, MISO=GPIO12, SCLK=GPIO13, CS=GPIO14, INT=GPIO10, RST=GPIO9
//!
//! Note: Previous bring-up used pure-Rust `w5500-ll`; switched to esp_eth
//! for TCP/IP+TLS support. W5500 SPI Ethernet must be enabled in sdkconfig.defaults.

use anyhow::{Context, Result};
use esp_idf_svc::eth::{BlockingEth, EspEth, EthDriver, SpiEth, SpiEthChipset};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::{Gpio10, Gpio11, Gpio12, Gpio13, Gpio14, Gpio9};
use esp_idf_svc::hal::spi::{Dma, SpiDriver, SpiDriverConfig, SPI2};
use esp_idf_svc::hal::units::FromValueType;

/// Wrapper keeping the initialized Ethernet alive. If dropped, the interface shuts down,
/// so it must be kept alive throughout `main`.
pub struct Eth<'d> {
    _eth: BlockingEth<EspEth<'d, SpiEth<SpiDriver<'d>>>>,
}

/// Initializes Ethernet. Returns: (handle, link_up).
///
/// IMPORTANT: If link/DHCP is unavailable, does NOT return Err; returns (handle, false).
/// Because if the handle is dropped, esp-idf-hal SpiDriver::drop panics with
/// `spi_bus_free().unwrap()` (the esp_eth SPI device is still attached to the bus -> INVALID_STATE).
/// The caller must keep this handle ALIVE (never drop it) even when falling back to WiFi.
#[allow(clippy::too_many_arguments)]
pub fn start<'d>(
    spi2: SPI2<'d>,
    sclk: Gpio13<'d>,
    mosi: Gpio11<'d>,
    miso: Gpio12<'d>,
    cs: Gpio14<'d>,
    int: Gpio10<'d>,
    rst: Gpio9<'d>,
    sysloop: EspSystemEventLoop,
) -> Result<(Eth<'d>, bool)> {
    // SPI bus that W5500 sits on (esp_eth adds its own device).
    // DMA is required: W5500 sends a full Ethernet frame (~1.5KB) in a single transfer;
    // default Dma::Disabled is limited to ~64 bytes ("txdata > host maximum").
    let spi = SpiDriver::new(
        spi2,
        sclk,
        mosi,
        Some(miso),
        &SpiDriverConfig::new().dma(Dma::Auto(4096)),
    )
    .context("Failed to create SPI bus")?;

    let driver = EthDriver::new_spi(
        spi,
        int,
        Some(cs),
        Some(rst),
        SpiEthChipset::W5500,
        20.MHz().into(), // Safe SPI speed for W5500
        None,            // MAC: eFuse factory MAC is used
        None,            // phy_addr
        sysloop.clone(),
    )
    .context("Failed to initialize W5500 EthDriver")?;

    let eth = EspEth::wrap(driver).context("EspEth wrap")?;
    let mut eth = BlockingEth::wrap(eth, sysloop).context("BlockingEth wrap")?;

    log::info!("Initializing Ethernet (W5500)...");
    eth.start().context("eth start")?;

    // Short wait for Link + DHCP (10s). If it times out, falls back to WiFi.
    log::info!("Waiting for Ethernet link + DHCP (10s)...");
    let mut up = false;
    for i in 1..=5 {
        if eth.is_up().unwrap_or(false) {
            up = true;
            break;
        }
        log::info!("... waiting for Ethernet ({}s)", i * 2);
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    if up {
        match eth.eth().netif().get_ip_info() {
            Ok(ip) => log::info!("Ethernet ready. IP: {ip:?}"),
            Err(e) => log::warn!("Ethernet is up but IP could not be read: {e}"),
        }
    } else {
        log::warn!("No Ethernet link/DHCP (10s); falling back to WiFi.");
    }

    // Note: Even if up=false, the handle is NOT dropped (panic prevention) — the caller keeps it.
    Ok((Eth { _eth: eth }, up))
}
