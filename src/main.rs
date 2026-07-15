//! Hardware bring-up for the Waveshare ESP32-S3-ETH board (see `../hardware/README.md`).
//!
//! Brings the on-board W5500 Ethernet chip up over SPI and reports its
//! version register and PHY link status. Pin assignments below come from
//! `../hardware/pins.png`, not a datasheet default:
//!   W5500 (SPI2/FSPI): MOSI = GPIO11, MISO = GPIO12, SCLK = GPIO13,
//!                       CS = GPIO14, INT = GPIO10, RST = GPIO9

#![no_std]
#![no_main]

use embedded_hal_bus::spi::ExclusiveDevice;
use esp_backtrace as _;
use esp_hal::{
    delay::Delay,
    gpio::{Level, Output, OutputConfig},
    main,
    spi::{
        master::{Config as SpiConfig, Spi},
        Mode,
    },
    time::Rate,
};
use w5500_ll::{
    eh1::{reset, vdm::W5500},
    LinkStatus, Registers,
};

esp_bootloader_esp_idf::esp_app_desc!();

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let mut delay = Delay::new();

    let mut rst = Output::new(peripherals.GPIO9, Level::High, OutputConfig::default());
    let cs = Output::new(peripherals.GPIO14, Level::High, OutputConfig::default());

    let spi_bus = Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(20))
            .with_mode(Mode::_0),
    )
    .unwrap()
    .with_sck(peripherals.GPIO13)
    .with_mosi(peripherals.GPIO11)
    .with_miso(peripherals.GPIO12);

    let spi_device = ExclusiveDevice::new_no_delay(spi_bus, cs).unwrap();
    let mut w5500 = W5500::new(spi_device);

    reset(&mut rst, &mut delay).unwrap();

    let version = w5500.version().unwrap();
    esp_println::println!("W5500 VERSIONR = 0x{:02X} (expected 0x04)", version);

    loop {
        let link = w5500.phycfgr().unwrap().lnk();
        esp_println::println!(
            "Ethernet link: {}",
            if link == LinkStatus::Up { "up" } else { "down" }
        );
        delay.delay_millis(1000);
    }
}
