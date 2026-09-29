//! The OTA job whose outcome is still open, kept across the reboot.
//!
//! The image that installs an update cannot know whether it will work, so it
//! records the job here and the next boot reads it back to report the outcome
//! (`gateway_core::jobs::settle`). The record is written after the image is
//! verified but before it is activated, so the new image can never boot
//! without it: a power cut any earlier leaves the running image active, and
//! the Jobs service simply offers the job again.
//!
//! It lives in the default `nvs` partition, next to the device identity, not
//! in the `opcua` one: it is a few hundred bytes written once per update, and
//! has nothing to do with the OPC UA configuration that partition caches.

use anyhow::{anyhow, Context, Result};
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};

use gateway_core::jobs::PendingUpdate;

/// NVS namespace; at most 15 characters.
const NAMESPACE: &str = "ota_job";
const KEY_PENDING: &str = "pending";

/// Largest record we will read back. A real one is about 150 B; the bound
/// keeps a corrupt length on flash from driving the allocation.
const MAX_RECORD_BYTES: usize = 512;

/// Read/write access to the pending OTA job.
pub struct JobStore {
    nvs: EspNvs<NvsDefault>,
}

impl JobStore {
    /// Opens (and creates if needed) the `ota_job` namespace.
    pub fn new(partition: EspDefaultNvsPartition) -> Result<Self> {
        let nvs = EspNvs::new(partition, NAMESPACE, true)
            .with_context(|| format!("opening NVS namespace {NAMESPACE:?}"))?;
        Ok(Self { nvs })
    }

    /// The pending job, if there is one.
    ///
    /// An unreadable record is discarded rather than reported: it must not
    /// keep the Jobs plane from ever taking work again. If the job it named
    /// was already installed, the Jobs service still has its outcome open and
    /// offers it again, and the `rebooting` status details settle it from
    /// there.
    pub fn load(&self) -> Option<PendingUpdate> {
        match self.try_load() {
            Ok(pending) => pending,
            Err(e) => {
                log::warn!("discarding unusable OTA job record: {e:#}");
                if let Err(e) = self.clear() {
                    log::warn!("could not remove it: {e:#}");
                }
                None
            }
        }
    }

    fn try_load(&self) -> Result<Option<PendingUpdate>> {
        let Some(len) = self.nvs.blob_len(KEY_PENDING)? else {
            return Ok(None);
        };
        if len > MAX_RECORD_BYTES {
            return Err(anyhow!(
                "record is {len} B, over the {MAX_RECORD_BYTES} B we accept"
            ));
        }
        let mut buf = vec![0u8; len];
        let Some(raw) = self.nvs.get_blob(KEY_PENDING, &mut buf)? else {
            return Ok(None);
        };
        let pending: PendingUpdate =
            serde_json::from_slice(raw).context("record is not valid JSON")?;
        if !pending.is_well_formed() {
            return Err(anyhow!(
                "record names an invalid job id {:?}",
                pending.job_id
            ));
        }
        Ok(Some(pending))
    }

    /// Records `pending`, replacing any earlier record.
    pub fn save(&self, pending: &PendingUpdate) -> Result<()> {
        let raw = serde_json::to_vec(pending)?;
        if raw.len() > MAX_RECORD_BYTES {
            return Err(anyhow!(
                "record is {} B, over the {MAX_RECORD_BYTES} B budget",
                raw.len()
            ));
        }
        self.nvs
            .set_blob(KEY_PENDING, &raw)
            .context("writing the OTA job record")?;
        log::info!(
            "recorded OTA job {} ({} -> {} in {})",
            pending.job_id,
            pending.from_version,
            pending.target_version,
            pending.target_slot
        );
        Ok(())
    }

    /// Removes the record. Not having one is not an error.
    pub fn clear(&self) -> Result<()> {
        self.nvs
            .remove(KEY_PENDING)
            .context("removing the OTA job record")?;
        Ok(())
    }
}
