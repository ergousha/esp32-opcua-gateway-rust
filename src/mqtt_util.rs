//! esp-idf-svc MQTT istemcisi icin ince sarmalayici: mutual-TLS istemci kurar
//! ve olaylari (Connected / Disconnected / Message) bir mpsc kanalina aktarir.
//!
//! Not: MqttClientConfiguration sertifikalar icin `X509<'static>` ister — bu
//! yuzden calisma zamaninda (NVS'den) gelen PEM'leri Box::leak ile 'static'a
//! yukseltiyoruz. Bu kimlikler program boyu yasadigindan sizinti PoC'de sorun
//! degil (cihaz tek kimlikle calisir).

use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use anyhow::{Context, Result};
use esp_idf_svc::mqtt::client::{
    EspMqttClient, EventPayload, MqttClientConfiguration, MqttProtocolVersion, QoS,
};
use esp_idf_svc::tls::X509;

/// Callback'ten ana goreve aktarilan olaylar (veriler sahiplenilmis kopyalar).
#[derive(Debug)]
pub enum MqttEvent {
    Connected,
    Disconnected,
    Message { topic: String, data: Vec<u8> },
}

/// Kurulan istemciyi ve olay kanalini birlikte tutar.
pub struct MqttSession {
    pub client: EspMqttClient<'static>,
    pub events: Receiver<MqttEvent>,
}

/// TLS kimlik bilgileri. Tek referans olarak gecirilir: connect'e 5 ayri &str
/// gecmek xtensa'da fat-pointer argumanlarinin register sinirini asip yanlis
/// okunmasina yol aciyordu; struct referansi bunu onler.
pub struct Creds<'a> {
    pub root_ca: &'a str,
    pub client_cert: &'a str,
    pub private_key: &'a str,
}

/// PEM string'i (nul'suz olabilir) 'static X509'a yukseltir.
fn static_x509(pem: &str) -> X509<'static> {
    let mut bytes = pem.as_bytes().to_vec();
    if bytes.last() != Some(&0) {
        bytes.push(0);
    }
    let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    X509::pem_until_nul(leaked)
}

/// Verilen kimlikle IoT Core'a TLS uzerinden baglanan bir istemci kurar.
/// Baglanti asenkron kurulur; cagiran `MqttEvent::Connected` beklemeli.
pub fn connect(url: &str, client_id: &str, creds: &Creds) -> Result<MqttSession> {
    let root_ca = creds.root_ca;
    let client_cert = creds.client_cert;
    let private_key = creds.private_key;

    log::info!(
        "connect: ca_len={} cert_len={} key_len={} free_heap={}",
        root_ca.len(),
        client_cert.len(),
        private_key.len(),
        unsafe { esp_idf_svc::sys::esp_get_free_heap_size() }
    );

    let (tx, rx) = channel::<MqttEvent>();

    let conf = MqttClientConfiguration {
        client_id: Some(client_id),
        protocol_version: Some(MqttProtocolVersion::V3_1_1),
        keep_alive_interval: Some(Duration::from_secs(60)),
        // create/accepted yaniti (cert+key+token) 1KB'yi asar -> buffer buyutuldu.
        buffer_size: 4096,
        out_buffer_size: 2048,
        server_certificate: Some(static_x509(root_ca)),
        client_certificate: Some(static_x509(client_cert)),
        private_key: Some(static_x509(private_key)),
        ..Default::default()
    };

    let client = EspMqttClient::new_cb(url, &conf, move |event| {
        match event.payload() {
            EventPayload::Connected(_) => {
                let _ = tx.send(MqttEvent::Connected);
            }
            EventPayload::Disconnected => {
                let _ = tx.send(MqttEvent::Disconnected);
            }
            EventPayload::Received { topic, data, .. } => {
                // topic yalnizca ilk chunk'ta gelir; buffer yeterince buyuk
                // oldugundan mesajlar tek parca gelir.
                if let Some(t) = topic {
                    let _ = tx.send(MqttEvent::Message {
                        topic: t.to_string(),
                        data: data.to_vec(),
                    });
                }
            }
            _ => {}
        }
    })
    .context("MQTT istemcisi kurulamadi")?;

    Ok(MqttSession { client, events: rx })
}

pub const QOS1: QoS = QoS::AtLeastOnce;
