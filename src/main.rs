//! Hardware bring-up for the Waveshare ESP32-S3-ETH board (see `../hardware/README.md`).
//!
//! Brings the on-board W5500 Ethernet chip up over SPI and reports its
//! version register and PHY link status. Pin assignments below come from
//! `../hardware/pins.png`, not a datasheet default:
//!   W5500 (SPI2/FSPI): MOSI = GPIO11, MISO = GPIO12, SCLK = GPIO13,
//!                       CS = GPIO14, INT = GPIO10, RST = GPIO9

use esp_idf_svc::hal::{
    delay::Delay,
    gpio::PinDriver,
    peripherals::Peripherals,
    spi::{config::Config as SpiConfig, SpiDeviceDriver, SpiDriverConfig},
    units::FromValueType,
};
use w5500_ll::{
    eh1::{reset, vdm::W5500},
    LinkStatus, Registers,
};

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let mut delay = Delay::new_default();

    let mut rst = PinDriver::output(peripherals.pins.gpio9)?;

    let spi_config = SpiConfig::new()
        .baudrate(20.MHz().into())
        .data_mode(embedded_hal::spi::MODE_0);
    let spi_device = SpiDeviceDriver::new_single(
        peripherals.spi2,
        peripherals.pins.gpio13,       // SCLK
        peripherals.pins.gpio11,       // MOSI
        Some(peripherals.pins.gpio12), // MISO
        Some(peripherals.pins.gpio14), // CS
        &SpiDriverConfig::new(),
        &spi_config,
    )?;
    let mut w5500 = W5500::new(spi_device);

    reset(&mut rst, &mut delay).map_err(|e| anyhow::anyhow!("W5500 reset failed: {e:?}"))?;

    let version = w5500
        .version()
        .map_err(|e| anyhow::anyhow!("W5500 VERSIONR read failed: {e:?}"))?;
    log::info!("W5500 VERSIONR = 0x{version:02X} (expected 0x04)");

    loop {
        let link = w5500
            .phycfgr()
            .map_err(|e| anyhow::anyhow!("W5500 PHYCFGR read failed: {e:?}"))?
            .lnk();
        log::info!(
            "Ethernet link: {}",
            if link == LinkStatus::Up { "up" } else { "down" }
        );
        delay.delay_ms(1000);
    }
}
