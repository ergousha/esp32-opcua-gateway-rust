//! W5500 Ethernet'i ESP-IDF'in dahili esp_eth SPI surucusuyle lwIP netif
//! olarak baslatir; DHCP ile IP alinana kadar bloklar.
//!
//! Pinler `../hardware/pins.png`'den (SPI2/FSPI):
//!   MOSI=GPIO11, MISO=GPIO12, SCLK=GPIO13, CS=GPIO14, INT=GPIO10, RST=GPIO9
//!
//! Not: Eski bring-up saf-Rust `w5500-ll` kullaniyordu; TCP/IP+TLS icin
//! esp_eth'e gecildi. sdkconfig.defaults'ta W5500 SPI Ethernet acilmali.

use anyhow::{Context, Result};
use esp_idf_svc::eth::{BlockingEth, EspEth, EthDriver, SpiEth, SpiEthChipset};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::{Gpio10, Gpio11, Gpio12, Gpio13, Gpio14, Gpio9};
use esp_idf_svc::hal::spi::{Dma, SpiDriver, SpiDriverConfig, SPI2};
use esp_idf_svc::hal::units::FromValueType;

/// Baslatilmis Ethernet'i canli tutan sarmalayici. Drop edilirse arabirim kapanir,
/// bu yuzden `main` boyunca yasatilmali.
pub struct Eth<'d> {
    _eth: BlockingEth<EspEth<'d, SpiEth<SpiDriver<'d>>>>,
}

/// Ethernet'i baslatir. Donus: (handle, link_up).
///
/// ONEMLI: link/DHCP yoksa Err DONMEZ; (handle, false) doner. Cunku handle drop
/// edilirse esp-idf-hal SpiDriver::drop `spi_bus_free().unwrap()` ile panikler
/// (esp_eth SPI cihazi hala bus'a bagli -> INVALID_STATE). Cagiran, WiFi'ye
/// duserken bile bu handle'i CANLI tutmali (asla drop etmemeli).
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
    // W5500'un uzerinde oturdugu SPI veri yolu (esp_eth kendi cihazini ekler).
    // DMA sart: W5500 tam Ethernet frame'i (~1.5KB) tek transferde gonderir;
    // varsayilan Dma::Disabled ~64 baytla sinirli ("txdata > host maximum").
    let spi = SpiDriver::new(
        spi2,
        sclk,
        mosi,
        Some(miso),
        &SpiDriverConfig::new().dma(Dma::Auto(4096)),
    )
    .context("SPI veri yolu olusturulamadi")?;

    let driver = EthDriver::new_spi(
        spi,
        int,
        Some(cs),
        Some(rst),
        SpiEthChipset::W5500,
        20.MHz().into(), // W5500 icin guvenli SPI hizi
        None,            // MAC: eFuse fabrika MAC'i kullanilir
        None,            // phy_addr
        sysloop.clone(),
    )
    .context("W5500 EthDriver baslatilamadi")?;

    let eth = EspEth::wrap(driver).context("EspEth wrap")?;
    let mut eth = BlockingEth::wrap(eth, sysloop).context("BlockingEth wrap")?;

    log::info!("Ethernet baslatiliyor (W5500)...");
    eth.start().context("eth start")?;

    // Link + DHCP icin kisa bekleme (10s). Suresi dolarsa WiFi'ye dusulur.
    log::info!("Ethernet link + DHCP bekleniyor (10s)...");
    let mut up = false;
    for i in 1..=5 {
        if eth.is_up().unwrap_or(false) {
            up = true;
            break;
        }
        log::info!("... Ethernet bekleniyor ({}s)", i * 2);
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    if up {
        match eth.eth().netif().get_ip_info() {
            Ok(ip) => log::info!("Ethernet hazir. IP: {ip:?}"),
            Err(e) => log::warn!("Ethernet up ama IP okunamadi: {e}"),
        }
    } else {
        log::warn!("Ethernet link/DHCP yok (10s); WiFi'ye dusulecek.");
    }

    // Not: up=false olsa bile handle drop EDILMEZ (panik onlemi) — cagiran tutar.
    Ok((Eth { _eth: eth }, up))
}
