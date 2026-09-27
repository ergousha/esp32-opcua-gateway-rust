//! Persistent cache of the last successfully applied configuration.
//!
//! On boot the device applies whatever it last ran before it has heard from
//! the cloud. A gateway that only starts collecting once AWS answers is a
//! gateway that stops collecting whenever the WAN link is down, which is
//! exactly when local data matters most.
//!
//! The cache lives in its own `opcua` NVS partition (52 KiB, see
//! `partitions.csv`) rather than the default `nvs` partition, so a large tag
//! bundle can never crowd out the provisioned device identity.

use anyhow::{anyhow, Context, Result};
use esp_idf_svc::nvs::{EspCustomNvs, EspNvsPartition, NvsCustom};

use gateway_core::codec::{hex_eq_ignore_case, sha256_hex};
use gateway_core::settings::DesiredSettings;
use gateway_core::MAX_BUNDLE_BYTES;

/// Partition label; must match the `opcua` row in `partitions.csv`.
const PARTITION: &str = "opcua";
/// NVS namespace inside that partition.
const NAMESPACE: &str = "cfg";

const KEY_SETTINGS: &str = "settings";
const KEY_BUNDLE: &str = "bundle";

/// Largest settings document we will store.
const MAX_SETTINGS_BYTES: usize = 2 * 1024;

/// A configuration recovered from flash.
pub struct CachedConfig {
    /// The settings document, exactly as it was validated.
    pub settings: DesiredSettings,
    /// The raw bundle bytes, so the digest can be re-verified on load.
    pub bundle: Vec<u8>,
}

/// Read/write access to the persisted configuration.
pub struct SettingsStore {
    nvs: EspCustomNvs,
}

impl SettingsStore {
    /// Opens (and creates if needed) the `opcua` NVS namespace.
    pub fn new() -> Result<Self> {
        let partition = EspNvsPartition::<NvsCustom>::take(PARTITION)
            .with_context(|| format!("opening NVS partition {PARTITION:?}"))?;
        let nvs = EspCustomNvs::new(partition, NAMESPACE, true)
            .with_context(|| format!("opening NVS namespace {NAMESPACE:?}"))?;
        Ok(Self { nvs })
    }

    /// Loads the cached configuration.
    ///
    /// Anything inconsistent — missing half, bad JSON, digest mismatch — is
    /// reported as "no cache" rather than as an error: a corrupt cache must
    /// degrade into a cloud-only boot, not into a boot loop.
    pub fn load(&self) -> Option<CachedConfig> {
        match self.try_load() {
            Ok(cfg) => cfg,
            Err(e) => {
                log::warn!("discarding unusable cached OPC UA config: {e:#}");
                None
            }
        }
    }

    /// Length of a stored blob, refusing anything larger than we would accept.
    ///
    /// The bound matters: the length comes from flash, and trusting it would
    /// let a corrupt entry drive the allocation below.
    fn blob_len_within(&self, key: &str, max: usize) -> Result<Option<usize>> {
        let Some(len) = self.nvs.blob_len(key)? else {
            return Ok(None);
        };
        if len == 0 {
            return Ok(None);
        }
        if len > max {
            return Err(anyhow!(
                "cached {key} is {len} B, over the {max} B we accept"
            ));
        }
        Ok(Some(len))
    }

    fn try_load(&self) -> Result<Option<CachedConfig>> {
        // Size each buffer to the blob actually stored rather than to the
        // maximum we would accept. `load` is called again whenever a shadow
        // delta arrives, by which point TLS and the OPC UA session hold most
        // of the heap; asking for the full 10 KiB there aborts the process,
        // even though a real bundle is a few hundred bytes.
        let Some(settings_len) = self.blob_len_within(KEY_SETTINGS, MAX_SETTINGS_BYTES)? else {
            return Ok(None);
        };
        let mut settings_buf = vec![0u8; settings_len];
        let Some(raw_settings) = self.nvs.get_blob(KEY_SETTINGS, &mut settings_buf)? else {
            return Ok(None);
        };
        let settings: DesiredSettings =
            serde_json::from_slice(raw_settings).context("cached settings are not valid JSON")?;

        let Some(bundle_len) = self.blob_len_within(KEY_BUNDLE, MAX_BUNDLE_BYTES)? else {
            return Ok(None);
        };
        let mut bundle_buf = vec![0u8; bundle_len];
        let Some(bundle) = self.nvs.get_blob(KEY_BUNDLE, &mut bundle_buf)? else {
            return Ok(None);
        };
        let bundle = bundle.to_vec();

        // The two blobs are written separately, so a power cut between them
        // can leave a bundle that does not belong to the settings.
        if !hex_eq_ignore_case(&sha256_hex(&bundle), &settings.cfg.sha256) {
            return Err(anyhow!("cached bundle does not match cached settings"));
        }
        settings
            .validate()
            .map_err(|e| anyhow!("cached settings are no longer valid: {e}"))?;

        Ok(Some(CachedConfig { settings, bundle }))
    }

    /// Persists a configuration.
    ///
    /// The bundle is written before the settings, so the settings blob (which
    /// carries the digest) is only ever committed once the bundle it points at
    /// is already on flash. That ordering is what makes the mismatch check in
    /// [`Self::load`] sufficient.
    pub fn save(&mut self, settings: &DesiredSettings, bundle: &[u8]) -> Result<()> {
        if bundle.len() > MAX_BUNDLE_BYTES {
            return Err(anyhow!(
                "bundle is {} B, NVS budget is {MAX_BUNDLE_BYTES} B",
                bundle.len()
            ));
        }
        let raw_settings = serde_json::to_vec(settings)?;
        if raw_settings.len() > MAX_SETTINGS_BYTES {
            return Err(anyhow!(
                "settings are {} B, NVS budget is {MAX_SETTINGS_BYTES} B",
                raw_settings.len()
            ));
        }

        // Flash endurance: a device that re-receives the same retained bundle
        // on every reconnect must not rewrite the sector every time.
        if self.is_current(settings, bundle) {
            log::debug!("cached OPC UA config already up to date; skipping NVS write");
            return Ok(());
        }

        self.nvs.set_blob(KEY_BUNDLE, bundle)?;
        self.nvs.set_blob(KEY_SETTINGS, &raw_settings)?;
        log::info!(
            "cached OPC UA config v{} ({} B bundle) to NVS",
            settings.cfg.v,
            bundle.len()
        );
        Ok(())
    }

    fn is_current(&self, settings: &DesiredSettings, bundle: &[u8]) -> bool {
        let Ok(Some(existing_len)) = self.nvs.blob_len(KEY_BUNDLE) else {
            return false;
        };
        if existing_len != bundle.len() {
            return false;
        }
        match self.try_load() {
            Ok(Some(cached)) => {
                cached.settings.cfg.v == settings.cfg.v
                    && hex_eq_ignore_case(&cached.settings.cfg.sha256, &settings.cfg.sha256)
                    && cached.settings == *settings
            }
            _ => false,
        }
    }
}
