//! Normal calisma: cihaz kendi (provisioned) kimligiyle IoT Core'a baglanip
//! arka planda MQTT olaylarini dinler.

use std::thread::sleep;
use std::time::Duration;

use anyhow::{bail, Result};

use crate::config;
use crate::device_id::{self, DeviceIdentity};
use crate::mqtt_util::{self, MqttEvent};

/// Cihaz kimligiyle baglanip sonsuz dinleme dongusune girer.
pub fn run(id: &DeviceIdentity) -> Result<()> {
    log::info!(
        "Sadece baglanti modu. thing={}",
        id.thing_name
    );

    let mut session = mqtt_util::connect(
        &config::mqtt_url(),
        &id.thing_name, // client_id == thingName (device policy boyle sinirliyor)
        &mqtt_util::Creds {
            root_ca: device_id::root_ca_pem(),
            client_cert: &id.cert_pem,
            private_key: &id.key_pem,
        },
    )?;

    // Baglantiyi bekle.
    loop {
        match session.events.recv_timeout(Duration::from_secs(30)) {
            Ok(MqttEvent::Connected) => break,
            Ok(MqttEvent::Disconnected) => bail!("cihaz baglantisi koptu"),
            Ok(_) => continue,
            Err(_) => bail!("cihaz baglanti zaman asimi"),
        }
    }
    log::info!("Cihaz kimligiyle baglanildi. Baglanti acik tutuluyor.");

    loop {
        // Arka planda gelen olaylari ( or. disconnect) bosalt.
        while let Ok(ev) = session.events.try_recv() {
            if matches!(ev, MqttEvent::Disconnected) {
                log::warn!("baglanti koptu; esp-mqtt otomatik yeniden baglanacak.");
            }
        }

        sleep(Duration::from_millis(1000));
    }
}

/// Provisioning + calisma arasindaki gecis noktasi (ileride komut/OTA icin
/// genisletilebilir).
#[allow(dead_code)]
pub fn context_note() -> &'static str {
    "cmd/<thing>/* topic'leri device policy'de subscribe icin acik"
}
