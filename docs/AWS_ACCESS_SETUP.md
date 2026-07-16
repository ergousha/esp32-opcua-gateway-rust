# AWS hesap erisimi + Terraform kurulumu

Bu rehber, PoC'yi deploy edebilmen icin **AWS'e nasil erisecegini** ve
**Terraform'u nasil calistiracagini** adim adim anlatir. Sirasiyla ilerle.

---

## 1. Gerekli araclar

```sh
# macOS (brew)
brew install terraform awscli
terraform version   # >= 1.5
aws --version       # v2
```

Ayrica Lambda paketi icin `python3` (paket zip'i Terraform `archive_provider`
ile otomatik olusturulur; ekstra bir sey gerekmez).

---

## 2. AWS hesabina erisim (kisisel hesap — IAM user access key)

Kisisel AWS hesabini kullaniyorsun. **Root** kullanicisiyla gunluk is yapma;
onun yerine bir **IAM user** olustur ve onun access key'iyle calis.

### 2.1 IAM user olustur (bir kez, konsoldan)

1. AWS Console → **IAM** → **Users** → **Create user** (or. `esp32-ztp-admin`).
2. Yetki: PoC icin **AdministratorAccess** managed policy'sini ekle (en kolayi;
   daraltilmis set icin bkz. asagisi).
3. Kullanici olustuktan sonra → **Security credentials** → **Create access key**
   → tip olarak **Command Line Interface (CLI)** sec → cikan
   **Access key ID** + **Secret access key** ikilisini kaydet (secret bir daha
   gosterilmez).

> Root hesapta MFA acik olsun; IAM user'da da MFA onerilir.

### 2.2 CLI'yi yapilandir

```sh
aws configure --profile esp32-ztp
# AWS Access Key ID:      <access key id>
# AWS Secret Access Key:  <secret access key>
# Default region name:    eu-central-1
# Default output format:  json
```

Her terminalde profili sec ve kimligi dogrula:

```sh
export AWS_PROFILE=esp32-ztp
aws sts get-caller-identity   # arn'de olusturdugun IAM user gorunmeli
```

> Kimlik bilgileri `~/.aws/credentials` altinda tutulur; bu dosyayi asla
> commit'leme. Access key sizarsa konsoldan hemen deactivate/delete et.

**Alternatif — env dosyasi:** `aws configure` yerine kimlik bilgilerini kabuga
env degiskeniyle de yukleyebilirsin. Repo koke `aws-env.sh.example` sablonu
konuldu:

```sh
cp aws-env.sh.example aws-env.sh   # gercek access key/secret'i doldur
source ./aws-env.sh                # her yeni terminalde
aws sts get-caller-identity        # dogrula
```

`aws-env.sh` `.gitignore`'da — gercek sirri icerir, commit'lenmez.

### Gereken IAM izinleri

Terraform su servislerde kaynak olusturur; IAM user'in bunlara yetkili olmali
(PoC icin `AdministratorAccess` en kolayi, ama asagidaki daraltilmis set de
yeter):

- `iot:*` (Fleet Provisioning template, policy, certificate, topic rule)
- `dynamodb:*` (tablo)
- `lambda:*` (hook fonksiyonu)
- `iam:*` (rol/policy — Terraform rol yaratir; `iam:PassRole` sart)
- `logs:*` (CloudWatch log gruplari)

---

## 3. (Opsiyonel ama onerilen) Uzak Terraform state

PoC icin local state yeterli. Ekip/tekrarlanabilirlik icin S3 backend:

```sh
# Bir kez: state bucket olustur (versiyonlama + sifreleme acik)
aws s3api create-bucket --bucket <sirket>-esp32-ztp-tfstate \
  --region eu-central-1 --create-bucket-configuration LocationConstraint=eu-central-1
aws s3api put-bucket-versioning --bucket <sirket>-esp32-ztp-tfstate \
  --versioning-configuration Status=Enabled
aws s3api put-bucket-encryption --bucket <sirket>-esp32-ztp-tfstate \
  --server-side-encryption-configuration '{"Rules":[{"ApplyServerSideEncryptionByDefault":{"SSEAlgorithm":"AES256"}}]}'
```

Sonra `terraform/versions.tf` icindeki `backend "s3"` blogunu ac (bucket adini
yaz) ve `terraform init -migrate-state` calistir. Kilit icin modern Terraform'da
`use_lockfile = true` (S3 native lock) yeterli — ayri DynamoDB lock tablosu
gerekmez.

---

## 4. Terraform ile deploy

```sh
cd terraform
cp terraform.tfvars.example terraform.tfvars   # gerekiyorsa duzenle (region vb.)

terraform init
terraform plan     # ne olusacagini incele
terraform apply    # onayla
```

Cikan onemli output'lar:

```sh
terraform output iot_endpoint                # firmware config::MQTT_ENDPOINT
terraform output provisioning_template_name  # firmware config::PROVISIONING_TEMPLATE
terraform output dynamodb_table              # seed script tablo adi
```

Claim sertifikalarini firmware'e cikar:

```sh
terraform output -raw claim_certificate_pem > ../certs/claim.crt.pem
terraform output -raw claim_private_key     > ../certs/claim.private.key
```

---

## 5. Maliyet

Hepsi serverless + on-demand; **bosta ~0 USD**:

| Servis | Model | PoC maliyeti |
| --- | --- | --- |
| IoT Core | mesaj/baglanti basina | binlerce mesaj = birkac cent |
| DynamoDB | PAY_PER_REQUEST | okuma/yazma basina; PoC'de ihmal edilebilir |
| Lambda | istek + GB-sn | provisioning basina 1 kisa cagri; free tier |
| CloudWatch Logs | GB + saklama | 7 gun retention; minimal |

> Ilk provisioning'te AWS bir cihaz sertifikasi uretir; sertifikanin kendisi
> ucretsizdir. Asil ucret MQTT mesaj hacmine baglidir.

---

## 6. Temizlik

```sh
cd terraform
terraform destroy
```

> Not: `terraform destroy` provisioning ile **cihazlar tarafindan sonradan
> uretilen** sertifika/thing'leri silmez (bunlari Terraform yonetmez). Onlari
> IoT Core konsolundan ya da `aws iot delete-thing` / `delete-certificate` ile
> temizleyin. Claim sertifikasi ve template Terraform tarafindan silinir.
