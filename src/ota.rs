use std::ffi::CStr;
use std::ptr;

use anyhow::{Context, Result};
use esp_idf_svc::http::client::{Configuration as HttpConfiguration, EspHttpConnection};
use esp_idf_svc::ota::EspOta;
use esp_idf_svc::sys::{
    esp_ota_get_last_invalid_partition, esp_ota_get_next_update_partition,
    esp_ota_get_running_partition, esp_ota_get_state_partition, esp_ota_img_states_t,
    esp_ota_img_states_t_ESP_OTA_IMG_NEW, esp_ota_img_states_t_ESP_OTA_IMG_PENDING_VERIFY,
    esp_partition_t, ESP_OK,
};
use gateway_core::jobs::BootFacts;
use log::{error, info};

/// Performs an Over-The-Air (OTA) update from the given HTTP(S) URL.
///
/// `before_activate` is called with the label of the slot the image went to,
/// once the image is written and verified and before that slot becomes the
/// boot slot. If it fails, the update is abandoned and the running image stays
/// the one that boots. Returns the slot label.
pub fn perform_ota(url: &str, before_activate: impl FnOnce(&str) -> Result<()>) -> Result<String> {
    info!("Starting OTA firmware download.");

    // Initialize HTTP connection for downloading the firmware
    let http_config = HttpConfiguration {
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        buffer_size_tx: Some(4096),
        buffer_size: Some(4096),
        ..Default::default()
    };

    let mut connection = EspHttpConnection::new(&http_config)?;
    connection.initiate_request(
        esp_idf_svc::http::Method::Get,
        url,
        &[("Accept", "application/octet-stream")],
    )?;
    connection.initiate_response()?;

    let status = connection.status();
    if status != 200 {
        error!("OTA download failed with HTTP status: {}", status);
        anyhow::bail!("HTTP status: {}", status);
    }

    // Initialize OTA API
    let mut ota = EspOta::new()?;
    // The partition `initiate_update` writes to.
    let slot = label(unsafe { esp_ota_get_next_update_partition(ptr::null()) })
        .context("no OTA partition to update")?;
    let mut update = ota.initiate_update()?;

    let mut buf = [0u8; 4096];
    let mut downloaded = 0;

    info!("Downloading and writing firmware to OTA partition {slot}...");
    loop {
        let bytes_read = connection.read(&mut buf)?;
        if bytes_read == 0 {
            break; // EOF
        }
        update.write(&buf[..bytes_read])?;
        downloaded += bytes_read;
        if downloaded % (4096 * 10) == 0 {
            info!("Downloaded {} bytes", downloaded);
        }
    }

    info!("Download complete. Total {} bytes.", downloaded);

    // Verify the image, then set the boot partition, with the caller's step in
    // between: `complete()` would do both at once.
    let finished = update.finish()?;
    before_activate(&slot).context("preparing to activate the new image")?;
    finished.activate()?;
    info!("OTA update successful. Ready for reboot.");

    Ok(slot)
}

/// Marks the current running firmware as valid so it won't rollback on next boot.
pub fn mark_valid() -> Result<()> {
    let mut ota = EspOta::new()?;
    ota.mark_running_slot_valid()?;
    info!("Firmware marked as valid (no rollback).");
    Ok(())
}

/// What this boot can tell about its own image, for settling a pending OTA
/// job. `valid` is the result of [`mark_valid`].
///
/// Never fails: a slot that cannot be read is reported as unknown, and
/// `gateway_core::jobs::settle` then decides on the version alone.
pub fn boot_facts(valid: &Result<()>) -> BootFacts {
    // Sound: both return a pointer into the static partition table, or null.
    let running_slot = label(unsafe { esp_ota_get_running_partition() }).unwrap_or_default();
    let invalid_slot = label(unsafe { esp_ota_get_last_invalid_partition() });
    BootFacts {
        running_version: env!("CARGO_PKG_VERSION").to_string(),
        running_slot,
        invalid_slot,
        valid_error: valid.as_ref().err().map(|e| format!("{e:#}")),
    }
}

/// True while the running image is a fresh OTA image that has not marked
/// itself valid, i.e. one the bootloader rolls back if the device resets now.
pub fn running_unverified() -> bool {
    let mut state: esp_ota_img_states_t = Default::default();
    // Sound: the running partition is never null in a booted app, and
    // `state` outlives the call. An image without an otadata record reports
    // ESP_ERR_NOT_FOUND, and is not unverified either.
    let err = unsafe { esp_ota_get_state_partition(esp_ota_get_running_partition(), &mut state) };
    err == ESP_OK
        && (state == esp_ota_img_states_t_ESP_OTA_IMG_PENDING_VERIFY
            || state == esp_ota_img_states_t_ESP_OTA_IMG_NEW)
}

/// Label of a partition from the partition table, e.g. `ota_1`.
///
/// Read directly rather than through `EspOta::get_*_slot`, which also parses
/// the image's app descriptor into a 24-byte version string and fails outright
/// when the image in the slot has a longer one. ESP-IDF allows 32, and the
/// version it records is `git describe` of the build tree, e.g.
/// `v0.0.1-5-gea84f5a-dirty`, which is already 23.
fn label(partition: *const esp_partition_t) -> Option<String> {
    // Sound: a non-null pointer from the OTA API points into the partition
    // table, which lives for the whole program, and ESP-IDF keeps `label`
    // NUL-terminated (17 bytes for at most 16 characters).
    let partition = unsafe { partition.as_ref() }?;
    let label = unsafe { CStr::from_ptr(partition.label.as_ptr()) };
    let label = label.to_string_lossy().into_owned();
    (!label.is_empty()).then_some(label)
}
