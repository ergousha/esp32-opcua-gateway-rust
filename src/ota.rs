use anyhow::Result;
use esp_idf_svc::http::client::{Configuration as HttpConfiguration, EspHttpConnection};
use esp_idf_svc::ota::EspOta;
use log::{error, info};

/// Performs an Over-The-Air (OTA) update from the given HTTP(S) URL.
pub fn perform_ota(url: &str) -> Result<()> {
    info!("Starting OTA update from: {}", url);

    // Initialize HTTP connection for downloading the firmware
    let http_config = HttpConfiguration {
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        buffer_size_tx: Some(1024),
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
    let mut update = ota.initiate_update()?;

    let mut buf = [0u8; 4096];
    let mut downloaded = 0;

    info!("Downloading and writing firmware to OTA partition...");
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

    // Complete the update and set boot partition
    update.complete()?;
    info!("OTA update successful. Ready for reboot.");

    Ok(())
}

/// Marks the current running firmware as valid so it won't rollback on next boot.
pub fn mark_valid() -> Result<()> {
    let mut ota = EspOta::new()?;
    ota.mark_running_slot_valid()?;
    info!("Firmware marked as valid (no rollback).");
    Ok(())
}
