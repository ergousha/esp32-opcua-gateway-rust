//! Cihaz kimligi: gomulu claim (bootstrap) sertifikasi + root CA, ve provisioning
//! sonucu elde edilen kalici cihaz sertifikasinin NVS'de saklanmasi.

use anyhow::{Context, Result};
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use esp_idf_svc::sys::esp_read_mac;

// --- Gomulu bootstrap kimligi (build sirasinda binary'ye gomulur) --------
// pem_until_nul NIL sonlu tampon bekler; concat! ile "\0" ekliyoruz.
const CLAIM_CERT: &str = concat!(include_str!("../certs/claim.crt.pem"), "\0");
const CLAIM_KEY: &str = concat!(include_str!("../certs/claim.private.key"), "\0");
const ROOT_CA: &str = concat!(include_str!("../certs/AmazonRootCA1.pem"), "\0");

pub fn claim_cert_pem() -> &'static str {
    CLAIM_CERT
}
pub fn claim_key_pem() -> &'static str {
    CLAIM_KEY
}
pub fn root_ca_pem() -> &'static str {
    ROOT_CA
}

// --- NVS anahtarlari -----------------------------------------------------
const NVS_NAMESPACE: &str = "device_id";
const KEY_CERT: &str = "cert";
const KEY_KEY: &str = "key";
const KEY_THING: &str = "thing";

// NVS str max ~4000 bayt; RSA 2048 PEM sertifika/anahtar buna sigar.
const BUF_LEN: usize = 3072;

/// Provisioning sonucu elde edilen kalici cihaz kimligi.
#[derive(Clone)]
pub struct DeviceIdentity {
    pub cert_pem: String,
    pub key_pem: String,
    pub thing_name: String,
}

/// NVS'ye erisim sarmalayicisi.
pub struct DeviceStore {
    nvs: EspNvs<NvsDefault>,
}

impl DeviceStore {
    pub fn new(part: EspDefaultNvsPartition) -> Result<Self> {
        let nvs = EspNvs::new(part, NVS_NAMESPACE, true).context("NVS namespace acilamadi")?;
        Ok(Self { nvs })
    }

    /// Cihaz daha once provisioned edildi mi? (NVS'de thing kaydi var mi?)
    pub fn exists(&self) -> bool {
        matches!(self.nvs.str_len(KEY_THING), Ok(Some(_)))
    }

    /// Kalici cihaz kimligini NVS'den okur.
    pub fn load(&self) -> Result<DeviceIdentity> {
        let mut cert_buf = vec![0u8; BUF_LEN];
        let mut key_buf = vec![0u8; BUF_LEN];
        let mut thing_buf = vec![0u8; THING_BUF_LEN];

        let cert = self
            .nvs
            .get_str(KEY_CERT, &mut cert_buf)?
            .context("NVS: cert yok")?
            .to_string();
        let key = self
            .nvs
            .get_str(KEY_KEY, &mut key_buf)?
            .context("NVS: key yok")?
            .to_string();
        let thing = self
            .nvs
            .get_str(KEY_THING, &mut thing_buf)?
            .context("NVS: thing yok")?
            .to_string();

        Ok(DeviceIdentity {
            cert_pem: cert,
            key_pem: key,
            thing_name: thing,
        })
    }

    /// Provisioning sonucu kimligi NVS'ye yazar (kalici).
    pub fn save(&mut self, id: &DeviceIdentity) -> Result<()> {
        self.nvs.set_str(KEY_CERT, &id.cert_pem)?;
        self.nvs.set_str(KEY_KEY, &id.key_pem)?;
        self.nvs.set_str(KEY_THING, &id.thing_name)?;
        Ok(())
    }

    /// (Test) kayitli kimligi siler; sonraki bootta yeniden provisioning.
    #[allow(dead_code)]
    pub fn erase(&mut self) -> Result<()> {
        let _ = self.nvs.remove(KEY_CERT)?;
        let _ = self.nvs.remove(KEY_KEY)?;
        let _ = self.nvs.remove(KEY_THING)?;
        Ok(())
    }
}

const THING_BUF_LEN: usize = 128;

/// Karti "AA:BB:CC:DD:EE:FF" ve "AABBCCDDEEFF" formatlarinda MAC dondurur.
/// eFuse'daki fabrika MAC'ini okur (esp_read_mac / ESP_MAC_ETH).
pub fn mac_addr() -> (String, String) {
    let mut mac = [0u8; 6];
    // ESP_MAC_ETH = 3 (Ethernet arabirimi icin turetilen MAC).
    unsafe {
        esp_read_mac(mac.as_mut_ptr(), esp_idf_svc::sys::esp_mac_type_t_ESP_MAC_ETH);
    }
    let colon = format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );
    let plain = format!(
        "{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );
    (colon, plain)
}
