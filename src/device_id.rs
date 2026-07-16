//! Device identity: embedded claim (bootstrap) certificate + root CA, and storing the
//! persistent device certificate obtained from provisioning in NVS.

use anyhow::{Context, Result};
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use esp_idf_svc::sys::esp_read_mac;

// --- Embedded bootstrap identity (embedded into binary during build) --------
// pem_until_nul expects a null-terminated buffer; we append "\0" with concat!.
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

// --- NVS keys ------------------------------------------------------------
const NVS_NAMESPACE: &str = "device_id";
const KEY_CERT: &str = "cert";
const KEY_KEY: &str = "key";
const KEY_THING: &str = "thing";

// NVS str max ~4000 bytes; RSA 2048 PEM cert/key fits in this.
const BUF_LEN: usize = 3072;

/// Persistent device identity obtained as a result of provisioning.
#[derive(Clone)]
pub struct DeviceIdentity {
    pub cert_pem: String,
    pub key_pem: String,
    pub thing_name: String,
}

/// NVS access wrapper.
pub struct DeviceStore {
    nvs: EspNvs<NvsDefault>,
}

impl DeviceStore {
    pub fn new(part: EspDefaultNvsPartition) -> Result<Self> {
        let nvs = EspNvs::new(part, NVS_NAMESPACE, true).context("Failed to open NVS namespace")?;
        Ok(Self { nvs })
    }

    /// Has the device been provisioned before? (Is there a thing record in NVS?)
    pub fn exists(&self) -> bool {
        matches!(self.nvs.str_len(KEY_THING), Ok(Some(_)))
    }

    /// Reads persistent device identity from NVS.
    pub fn load(&self) -> Result<DeviceIdentity> {
        let mut cert_buf = vec![0u8; BUF_LEN];
        let mut key_buf = vec![0u8; BUF_LEN];
        let mut thing_buf = vec![0u8; THING_BUF_LEN];

        let cert = self
            .nvs
            .get_str(KEY_CERT, &mut cert_buf)?
            .context("NVS: cert missing")?
            .to_string();
        let key = self
            .nvs
            .get_str(KEY_KEY, &mut key_buf)?
            .context("NVS: key missing")?
            .to_string();
        let thing = self
            .nvs
            .get_str(KEY_THING, &mut thing_buf)?
            .context("NVS: thing missing")?
            .to_string();

        Ok(DeviceIdentity {
            cert_pem: cert,
            key_pem: key,
            thing_name: thing,
        })
    }

    /// Writes the identity obtained from provisioning to NVS (persistently).
    pub fn save(&mut self, id: &DeviceIdentity) -> Result<()> {
        self.nvs.set_str(KEY_CERT, &id.cert_pem)?;
        self.nvs.set_str(KEY_KEY, &id.key_pem)?;
        self.nvs.set_str(KEY_THING, &id.thing_name)?;
        Ok(())
    }

    /// (Test) erases registered identity; next boot triggers re-provisioning.
    #[allow(dead_code)]
    pub fn erase(&mut self) -> Result<()> {
        let _ = self.nvs.remove(KEY_CERT)?;
        let _ = self.nvs.remove(KEY_KEY)?;
        let _ = self.nvs.remove(KEY_THING)?;
        Ok(())
    }
}

const THING_BUF_LEN: usize = 128;

/// Returns MAC in "AA:BB:CC:DD:EE:FF" and "AABBCCDDEEFF" formats.
/// Reads the factory MAC in eFuse (esp_read_mac / ESP_MAC_ETH).
pub fn mac_addr() -> (String, String) {
    let mut mac = [0u8; 6];
    // ESP_MAC_ETH = 3 (MAC derived for the Ethernet interface).
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
