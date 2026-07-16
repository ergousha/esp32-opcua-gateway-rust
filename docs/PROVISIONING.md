# Zero-Touch Provisioning — mimari ve akis

Yaklasim: **AWS IoT Fleet Provisioning by Claim**. Cihaz fabrikadan tum filo
icin **ortak** bir "claim" (bootstrap) sertifikasiyla cikar. Ilk baglantida
kendine ait **benzersiz** bir sertifika uretir; MAC + secret bir Lambda hook
tarafindan DynamoDB'ye karsi dogrulanir. Onaylanirsa IoT Core cihaz icin
`thing` + `certificate` + `policy` olusturur.

## Bilesenler

| Katman | Bilesen | Rol |
| --- | --- | --- |
| Cihaz | ESP32-S3-ETH (Rust) | claim ile baglanir, kendi kimligini alir, NVS'ye yazar |
| AWS | IoT Core Fleet Provisioning template | thing/cert/policy'yi tanimlar |
| AWS | Lambda pre-provisioning hook | MAC+secret'i DynamoDB'de dogrular (izin ver/reddet) |
| AWS | DynamoDB `*-device-registry` | izinli cihaz kayitlari (MAC, secret, allowed) |
| AWS | IoT Rule -> CloudWatch Logs | telemetriyi gozlemlemek icin |

## Akis diyagrami

```
  ESP32-S3                         AWS IoT Core                 Lambda        DynamoDB
     |                                  |                          |             |
     |-- TLS connect (CLAIM cert) ----->|                          |             |
     |-- pub $aws/certificates/create ->|                          |             |
     |<- accepted {certPem,key,token} --|  (benzersiz cert uretir) |             |
     |                                  |                          |             |
     |-- pub .../provision {token,      |                          |             |
     |         SerialNumber,MacAddress, |-- pre-provisioning hook ->|             |
     |         Secret} ---------------->|                          |-- get MAC ->|
     |                                  |                          |<- secret ---|
     |                                  |<- allowProvisioning:true-|             |
     |                                  | (thing+cert+policy olustu)             |
     |<- accepted {thingName} ----------|                          |             |
     |  (cert+key+thing NVS'ye yazilir) |                          |             |
     |                                  |                          |             |
     |== reconnect (DEVICE cert) ======>|                          |             |
     |-- pub dt/<thing>/data ---------->|  (IoT Rule -> CW Logs)   |             |
```

## Cihaz boot mantigi (`src/main.rs`)

1. **Ag**: once `eth::start(...)` — W5500'u esp_eth ile ayaga kaldir, link+DHCP
   icin 10s bekle. Link/lease yoksa `wifi::start(...)` ile WiFi'ye dus
   (`cfg.toml`). Not: Ethernet handle no-link durumunda bile CANLI tutulur
   (drop edilirse SpiDriver::drop panikler; bkz. asagidaki tuzaklar).
2. NVS'de kimlik var mi? (`DeviceStore::exists`)
   - **Var** → `load()` → dogrudan telemetri.
   - **Yok** → `provisioning::run()` → `save()` → telemetri.
3. `telemetry::run(&id)` — cihaz kimligiyle baglan, periyodik publish.

## Donanimda karsilasilan tuzaklar (cozuldu)

Bu PoC gercek ESP32-S3-ETH uzerinde dogrulandi; yol boyunca cozulen noktalar:

| Belirti | Kok neden | Cozum |
| --- | --- | --- |
| `spi_master: txdata transfer > host maximum` | SPI bus DMA kapali (~64B limit), W5500 ~1.5KB frame gonderir | `SpiDriverConfig::new().dma(Dma::Auto(4096))` (`eth.rs`) |
| WiFi'ye gecerken panik: `spi_bus_free().unwrap()` INVALID_STATE | No-link'te eth handle drop edilince esp_eth SPI cihazi hala bus'ta | Handle'i drop etme; `Net::Wifi { eth, .. }` icinde canli tut (`main.rs`) |
| `memory allocation of ~1GB failed` (connect'te) | 5 adet `&str` argumani xtensa register sinirini asinca fat-pointer'lar yanlis okundu | Sertifikalari tek `Creds` struct referansinda gecir (`mqtt_util.rs`) |
| Genel kararsizlik | TLS+MQTT+serde main task'ta, 8K stack az | `CONFIG_ESP_MAIN_TASK_STACK_SIZE=16384` |

MAC notu: cihaz kimligi olarak `ESP_MAC_ETH` kullanilir; bu, eFuse taban
MAC'inden turetilir (taban `..:4C` → ETH `..:4F`). Seed ederken cihazin ilk
bootta logladigi MAC'i kullanin.

## MQTT topic'leri

| Amac | Topic |
| --- | --- |
| Cert uret (istek) | `$aws/certificates/create/json` |
| Cert uret (yanit) | `$aws/certificates/create/json/{accepted,rejected}` |
| RegisterThing (istek) | `$aws/provisioning-templates/<template>/provision/json` |
| RegisterThing (yanit) | `$aws/provisioning-templates/<template>/provision/json/{accepted,rejected}` |
| Telemetri | `dt/<thingName>/data` |
| Komut (device policy'de acik) | `cmd/<thingName>/*` |

## Guvenlik modeli

- **Claim policy** (Terraform `iot.tf`): sadece `iot:Connect` + provisioning
  topic'leri. Claim cert ile telemetri **gonderilemez**.
- **Device policy**: politika degiskenleriyle her cihaz yalnizca **kendi**
  `dt/<thingName>/*` ve `cmd/<thingName>/*` topic'lerine erisir; client_id =
  thingName zorunlu.
- **Lambda hook**: MAC kaydi yoksa / `allowed=false` / secret uyusmuyorsa
  provisioning reddedilir. Sabit-zamanli secret karsilastirmasi kullanir.

## ESP32-S3 guvenlik ozellikleri (PoC vs uretim)

PoC seviyesinde (secilen): sertifikalar sifresiz NVS'de, eFuse **yakilmaz**.
Uretime giderken:

- **Flash Encryption** + **Secure Boot v2**: NVS/flash'taki cert+key'i ve
  firmware'i korur (eFuse yakma — geri alinamaz).
- **DS (Digital Signature) peripheral**: cihaz private key'i eFuse'da sifreli
  tutulur, RAM'e cikmadan TLS imzalama yapilir. `esp-idf` `esp_ds` API'si;
  provisioning'de CreateCertificateFromCsr akisina gecilir (CSR'yi DS ile
  imzalayip AWS'e gonderirsin), boylece private key hic cihazi terk etmez.
- **Per-device secret**: PoC'de ortak; uretimde cihaz basina benzersiz ve
  DS/eFuse ile korunmali.

## Sorun giderme

| Belirti | Olasi neden |
| --- | --- |
| `netif up olmadi` | Ethernet kablosu yok / DHCP sunucusu yok |
| `baglanti zaman asimi` (claim) | `MQTT_ENDPOINT` yanlis; claim cert/policy eksik; saat/TLS |
| Provisioning `REDDEDILDI` | MAC DynamoDB'de yok / `allowed=false` / secret uyusmuyor |
| `certificatePem yok` | buffer_size kucuk (kodda 4096) ya da JSON degil CBOR topic |
| Telemetri yok ama provision OK | device policy topic prefix ile `config::TELEMETRY_TOPIC_PREFIX` farkli |

Loglar: IoT tarafinda CloudWatch `/{project}/telemetry` ve
`/aws/lambda/{project}-pre-provisioning-hook`. Cihaz tarafinda seri monitor.

## Yeniden provisioning (test)

Cihazi sifirdan provision ettirmek icin NVS'yi temizle:

```sh
espflash erase-region 0x9000 0x6000   # nvs partition (varsayilan tablo)
# veya tum flash:
espflash erase-flash
```
