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
| `claim.crt.pem` | **Claim (bootstrap) certificate** — common across all devices | `terraform output` |
| `claim.private.key` | Claim private key (**secret**) | `terraform output` |

## Generating the Real Claim Identity

After running Terraform apply:

```sh
cd ../terraform
terraform output -raw claim_certificate_pem > ../certs/claim.crt.pem
terraform output -raw claim_private_key     > ../certs/claim.private.key
# Root CA (once):
curl -s https://www.amazontrust.com/repository/AmazonRootCA1.pem > ../certs/AmazonRootCA1.pem
```

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
