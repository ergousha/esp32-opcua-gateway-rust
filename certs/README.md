# Cihaz sertifikalari

Bu klasordeki dosyalar firmware'e `include_str!` ile **build sirasinda gomulur**
(`src/device_id.rs`).

Claim cert + key **GERCEK SIR** oldugundan `.gitignore`'dadir; repoda tutulmaz.
Temiz bir checkout'ta yoklarsa `build.rs` otomatik **placeholder** uretir —
boylece `cargo build`/CI kirilmaz (ama gercek kimlik flashlanana kadar TLS
calismaz). `AmazonRootCA1.pem` publiktir ve commit'lenir.

| Dosya | Ne | Kaynak |
| --- | --- | --- |
| `AmazonRootCA1.pem` | AWS IoT sunucu dogrulamasi (publik) | Amazon Trust — commit'lenebilir |
| `claim.crt.pem` | **Claim (bootstrap) sertifikasi** — tum cihazlarda ortak | `terraform output` |
| `claim.private.key` | Claim private key (**sir**) | `terraform output` |

## Gercek claim kimligini uretme

Terraform apply sonrasi:

```sh
cd ../terraform
terraform output -raw claim_certificate_pem > ../certs/claim.crt.pem
terraform output -raw claim_private_key     > ../certs/claim.private.key
# Root CA (bir kez):
curl -s https://www.amazontrust.com/repository/AmazonRootCA1.pem > ../certs/AmazonRootCA1.pem
```

## Gercek sirri commit'lememek

`certs/claim.crt.pem` ve `certs/claim.private.key` `.gitignore`'dadir; `git add`
onlari sahnelemez, `git status`'ta gorunmezler. Yani gercek anahtari yazsan bile
kazara commit'lenmezler. Dogrula:

```sh
git check-ignore -v certs/claim.private.key   # ignore edildigini gosterir
```

## Guvenlik notu (PoC vs uretim)

- **PoC**: claim kimligi + cihaz secret'i firmware'e/kaynaga gomulu. Kabul
  edilebilir cunku amac akisi gostermek.
- **Uretim**: claim private key tum filoyu riske atan paylasilan bir sirdir.
  Cihaz sertifikasi/anahtari ESP32-S3 **DS peripheral**'inde tutulmali, secure
  boot v2 + flash encryption ile eFuse'lar yakilmali. Claim'i mumkunse tek
  kullanimlik/kisa omurlu yapin ve provisioning sonrasi devre disi birakin.
