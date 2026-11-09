//! Adds or updates a device in the DynamoDB device registry.
//!
//! During Fleet Provisioning the device presents its MAC along with the claim
//! certificate, and the pre-provisioning Lambda hook checks it against this
//! table. Provisioning is REJECTED unless the device is registered here,
//! `allowed`, for the product whose template it uses, and proves itself: with
//! the shared secret (the gateway), or, for a device with a secure element
//! (Vaktus), with a certificate for the public key registered here.
//!
//! ```sh
//! source aws-env.sh
//! # gateway
//! cargo run -p seed-device --target host-tuple -- \
//!     --mac AA:BB:CC:DD:EE:FF --secret change-me-shared-secret
//! # Vaktus: the MAC and key it logs and shows on first boot
//! cargo run -p seed-device --target host-tuple -- \
//!     --product vaktus --mac 38:18:2B:84:56:F4 --public-key <128 hex digits>
//! ```
//!
//! The device logs its MAC on first boot (`Provisioning starting. MAC=…`).
//! Use that value: it is the *Ethernet* MAC, which differs from the base MAC
//! printed on the module (see `docs/FIRMWARE_INTEGRATION.md`).

use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};
use aws_config::Region;
use aws_sdk_dynamodb::config::ProvideCredentials;
use aws_sdk_dynamodb::types::AttributeValue;

/// Terraform's `<project_name>-device-registry`.
const DEFAULT_TABLE: &str = "esp32-ztp-device-registry";
const DEFAULT_REGION: &str = "eu-central-1";

const USAGE: &str = "\
usage: seed-device --mac MAC (--secret SECRET | --public-key HEX) [--product ID]
                   [--table NAME] [--region REGION] [--allowed true|false]

Adds a device to the DynamoDB registry the fleet-provisioning hook checks.
An existing entry for the same MAC is overwritten, which also resets
`provisioned` to false.

  --mac MAC         device MAC as logged on first boot, e.g. AA:BB:CC:DD:EE:FF
                    (case and `:`/`-` separators are normalised)
  --product ID      platform product the device enrols as (default gateway)
  --secret SECRET   shared secret, the same value as the firmware's DEVICE_SECRET
  --public-key HEX  the device's P-256 public key as its secure element reports
                    it: X||Y (128 hex digits) or 04||X||Y (130); `:` and spaces
                    are ignored. Required for products with a secure element
  --table NAME      DynamoDB table                  (default esp32-ztp-device-registry)
  --region REGION   AWS region                      (default eu-central-1)
  --allowed BOOL    whether provisioning is allowed (default true)";

/// The product a record belongs to when none is given; the hook treats records
/// without one the same way.
const DEFAULT_PRODUCT: &str = "gateway";

#[derive(Debug, PartialEq)]
struct Args {
    mac: String,
    product: String,
    secret: Option<String>,
    /// Uppercase hex of X||Y, 128 digits.
    public_key: Option<String>,
    table: String,
    region: String,
    allowed: bool,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args> {
    let (mut mac, mut secret, mut public_key) = (None, None, None);
    let mut product = DEFAULT_PRODUCT.to_string();
    let mut table = DEFAULT_TABLE.to_string();
    let mut region = DEFAULT_REGION.to_string();
    let mut allowed = true;

    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| anyhow!("{flag} needs a value"));
        match flag.as_str() {
            "--mac" => mac = Some(normalise_mac(&value()?)?),
            "--secret" => secret = Some(value()?),
            "--public-key" => public_key = Some(normalise_public_key(&value()?)?),
            "--product" => product = value()?,
            "--table" => table = value()?,
            "--region" => region = value()?,
            "--allowed" => {
                allowed = match value()?.as_str() {
                    "true" => true,
                    "false" => false,
                    other => bail!("--allowed must be true or false, got {other:?}"),
                }
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other:?}"),
        }
    }

    let mac = mac.ok_or_else(|| anyhow!("--mac is required"))?;
    if secret.as_deref() == Some("") {
        bail!("--secret must not be empty");
    }
    if secret.is_none() && public_key.is_none() {
        bail!("--secret or --public-key is required");
    }
    let id_ok = product.len() >= 2
        && product.len() <= 16
        && product.starts_with(|c: char| c.is_ascii_lowercase())
        && product
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !id_ok {
        bail!("--product {product:?}: 2-16 lowercase letters and digits, starting with a letter");
    }
    Ok(Args {
        mac,
        product,
        secret,
        public_key,
        table,
        region,
        allowed,
    })
}

/// Normalises a MAC to the exact string the firmware sends
/// (`device_id::mac_addr`): six uppercase hex octets separated by `:`.
///
/// The hook compares strings, so `aa:bb:…` or `AA-BB-…` in the table would
/// never match and the device would be rejected without saying why.
fn normalise_mac(input: &str) -> Result<String> {
    let hex: String = input.chars().filter(|c| !matches!(c, ':' | '-')).collect();
    if hex.len() != 12 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("{input:?} is not a MAC address; expected six hex octets, e.g. AA:BB:CC:DD:EE:FF");
    }
    let hex = hex.to_ascii_uppercase();
    let octets: Vec<&str> = (0..6).map(|i| &hex[i * 2..i * 2 + 2]).collect();
    Ok(octets.join(":"))
}

/// Normalises a P-256 public key to the uppercase hex of X||Y.
///
/// Accepts what an ATECC608 reports (64 bytes of X||Y) and the uncompressed
/// SEC1 form with its `04` prefix, with `:` and whitespace anywhere. Whether the
/// point is on the curve is checked by the hook, which refuses an invalid key.
fn normalise_public_key(input: &str) -> Result<String> {
    let hex: String = input
        .chars()
        .filter(|c| !matches!(c, ':' | ' ' | '\n' | '\r' | '\t'))
        .collect::<String>()
        .to_ascii_uppercase();
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("--public-key is not hex");
    }
    match hex.len() {
        128 => Ok(hex),
        130 if hex.starts_with("04") => Ok(hex[2..].to_string()),
        n => bail!("--public-key has {n} hex digits; expected 128 (X||Y) or 130 (04||X||Y)"),
    }
}

async fn run(args: Args) -> Result<()> {
    let config = aws_config::from_env()
        .region(Region::new(args.region.clone()))
        .load()
        .await;

    // Checked up front: without credentials the SDK fails later with a
    // dispatch error that does not say what is actually missing.
    let provider = config
        .credentials_provider()
        .ok_or_else(|| anyhow!("no AWS credentials provider is configured"))?;
    provider.provide_credentials().await.map_err(|e| {
        anyhow!(
            "AWS credentials not found ({e}). Run `source aws-env.sh`, or set AWS_PROFILE \
             to a configured profile (e.g. `aws configure --profile esp32-ztp`)."
        )
    })?;

    let mut put = aws_sdk_dynamodb::Client::new(&config)
        .put_item()
        .table_name(&args.table)
        .item("mac_address", AttributeValue::S(args.mac.clone()))
        .item("product", AttributeValue::S(args.product.clone()))
        .item("allowed", AttributeValue::Bool(args.allowed))
        .item("provisioned", AttributeValue::Bool(false));
    if let Some(secret) = &args.secret {
        put = put.item("secret", AttributeValue::S(secret.clone()));
    }
    if let Some(key) = &args.public_key {
        put = put.item("public_key", AttributeValue::S(key.clone()));
    }
    put.send()
        .await
        .map_err(|e| anyhow!("{}", aws_sdk_dynamodb::error::DisplayErrorContext(e)))
        .with_context(|| format!("DynamoDB PutItem into {}", args.table))?;

    println!(
        "OK: {} added to table ({}) as product {}, enrolling with {}. allowed={}",
        args.mac,
        args.table,
        args.product,
        if args.public_key.is_some() {
            "its public key"
        } else {
            "the secret"
        },
        args.allowed
    );
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("error: {e:#}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ERROR: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args> {
        parse_args(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn macs_are_normalised_to_the_firmware_format() {
        for input in [
            "AA:BB:CC:DD:EE:0F",
            "aa:bb:cc:dd:ee:0f",
            "AA-BB-CC-DD-EE-0F",
            "aabbccddee0f",
        ] {
            assert_eq!(
                normalise_mac(input).unwrap(),
                "AA:BB:CC:DD:EE:0F",
                "{input}"
            );
        }
    }

    #[test]
    fn malformed_macs_are_refused() {
        for input in [
            "",
            "AA:BB:CC:DD:EE",
            "AA:BB:CC:DD:EE:FF:00",
            "GG:BB:CC:DD:EE:FF",
        ] {
            assert!(normalise_mac(input).is_err(), "{input}");
        }
    }

    #[test]
    fn defaults_match_the_terraform_names() {
        let parsed = args(&["--mac", "aa:bb:cc:dd:ee:ff", "--secret", "s"]).unwrap();
        assert_eq!(
            parsed,
            Args {
                mac: "AA:BB:CC:DD:EE:FF".into(),
                product: "gateway".into(),
                secret: Some("s".into()),
                public_key: None,
                table: "esp32-ztp-device-registry".into(),
                region: "eu-central-1".into(),
                allowed: true,
            }
        );
    }

    const KEY: &str = "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF\
                       FEDCBA9876543210FEDCBA9876543210FEDCBA9876543210FEDCBA9876543210";

    #[test]
    fn a_secure_element_device_is_registered_with_its_key() {
        let with_colons: String = format!("04{}", KEY.to_lowercase())
            .as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join(":");
        for key in [KEY.to_string(), with_colons] {
            let parsed = args(&[
                "--product",
                "vaktus",
                "--mac",
                "38:18:2b:84:56:f4",
                "--public-key",
                &key,
            ])
            .unwrap();
            assert_eq!(parsed.product, "vaktus");
            assert_eq!(parsed.secret, None);
            assert_eq!(parsed.public_key.as_deref(), Some(KEY));
        }
    }

    #[test]
    fn malformed_keys_and_products_are_refused() {
        let mac = ["--mac", "AA:BB:CC:DD:EE:FF"];
        for bad_key in [
            &KEY[..126],
            &format!("05{KEY}"),
            &format!("{}ZZ", &KEY[..126]),
        ] {
            assert!(
                args(&[mac[0], mac[1], "--public-key", bad_key]).is_err(),
                "{bad_key}"
            );
        }
        for bad_product in ["", "v", "Vaktus", "vak tus", "1vaktus", "abcdefghijklmnopq"] {
            assert!(
                args(&[mac[0], mac[1], "--secret", "s", "--product", bad_product]).is_err(),
                "{bad_product:?}"
            );
        }
    }

    #[test]
    fn required_and_invalid_arguments_are_reported() {
        assert!(args(&["--secret", "s"]).is_err());
        assert!(args(&["--mac", "AA:BB:CC:DD:EE:FF"]).is_err());
        assert!(args(&["--mac", "AA:BB:CC:DD:EE:FF", "--secret", ""]).is_err());
        assert!(args(&[
            "--mac",
            "AA:BB:CC:DD:EE:FF",
            "--secret",
            "s",
            "--allowed",
            "yes"
        ])
        .is_err());
        let denied = args(&[
            "--mac",
            "AA:BB:CC:DD:EE:FF",
            "--secret",
            "s",
            "--allowed",
            "false",
        ]);
        assert!(!denied.unwrap().allowed);
    }
}
