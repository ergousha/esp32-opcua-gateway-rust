# Device Certificates

Files in this folder are **embedded during build** into the firmware using `include_str!`
(`src/device_id.rs`).

Since claim cert + key are **ACTUAL SECRETS**, they are in `.gitignore` and not stored in the repository.
If they do not exist on a clean checkout, `build.rs` automatically generates a **placeholder** —
this prevents the `cargo build`/CI from breaking (but TLS will not work until a real identity is flashed).
`AmazonRootCA1.pem` is public and committed.

| File | What | Source |
| --- | --- | --- |
| `AmazonRootCA1.pem` | AWS IoT server validation (public) | Amazon Trust — committable |
| `claim.crt.pem` | **Claim (bootstrap) certificate** — common across all devices | SSM `/esp32-ztp/poc/claim_certificate_pem` |
| `claim.private.key` | Claim private key (**secret**) | SSM `/esp32-ztp/poc/claim_private_key` |

## Generating the Real Claim Identity

`build.rs` fetches both from SSM when the AWS CLI has credentials. By hand
(the platform no longer exposes them as Terraform outputs):

```sh
aws ssm get-parameter --name /esp32-ztp/poc/claim_certificate_pem --with-decryption \
  --query Parameter.Value --output text > certs/claim.crt.pem
aws ssm get-parameter --name /esp32-ztp/poc/claim_private_key --with-decryption \
  --query Parameter.Value --output text > certs/claim.private.key
# Root CA (once):
curl -s https://www.amazontrust.com/repository/AmazonRootCA1.pem > certs/AmazonRootCA1.pem
```

The claim certificate is **INACTIVE** on the platform except during an
enrolment window (`scripts/claim-window.sh` in iot-platform-infra), and a
registry record enrols a device only once. A provisioned gateway never uses the
claim again; to re-enrol one, register it again with `seed-device`.

## Preventing Committing the Real Secret

`certs/claim.crt.pem` and `certs/claim.private.key` are in `.gitignore`; `git add`
will not stage them, and they won't appear in `git status`. So even if you write the real key,
they won't be committed by accident. Verify:

```sh
git check-ignore -v certs/claim.private.key   # shows that it is ignored
```

## Security Note (PoC vs Production)

- **PoC**: claim identity + device secret embedded in firmware/source. Acceptable
  because the goal is to demonstrate the flow.
- **Production**: the claim private key is a shared secret that risks the entire fleet.
  The device certificate/key must be kept in the ESP32-S3 **DS peripheral**, secure
  boot v2 + flash encryption should be enabled, and eFuses burned. If possible, make the claim
  one-time use/short-lived and disable it after provisioning.
