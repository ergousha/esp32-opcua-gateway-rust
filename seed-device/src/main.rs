//! Adds or updates a device in the DynamoDB device registry.
//!
//! During Fleet Provisioning the device presents its MAC and the shared secret
//! along with the claim certificate, and the pre-provisioning Lambda hook
//! checks them against this table. Provisioning is REJECTED unless the device
//! is registered here and `allowed`.
//!
//! ```sh
//! source aws-env.sh
//! cargo run -p seed-device --target host-tuple -- \
//!     --mac AA:BB:CC:DD:EE:FF --secret change-me-shared-secret
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
usage: seed-device --mac MAC --secret SECRET [--table NAME] [--region REGION] [--allowed true|false]

Adds a device to the DynamoDB registry the fleet-provisioning hook checks.
An existing entry for the same MAC is overwritten, which also resets
`provisioned` to false.

  --mac MAC         device MAC as logged on first boot, e.g. AA:BB:CC:DD:EE:FF
                    (case and `:`/`-` separators are normalised)
  --secret SECRET   shared secret, the same value as the firmware's DEVICE_SECRET
  --table NAME      DynamoDB table                  (default esp32-ztp-device-registry)
  --region REGION   AWS region                      (default eu-central-1)
  --allowed BOOL    whether provisioning is allowed (default true)";

#[derive(Debug, PartialEq)]
struct Args {
    mac: String,
    secret: String,
    table: String,
    region: String,
    allowed: bool,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args> {
    let (mut mac, mut secret) = (None, None);
    let mut table = DEFAULT_TABLE.to_string();
    let mut region = DEFAULT_REGION.to_string();
    let mut allowed = true;

    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| anyhow!("{flag} needs a value"));
        match flag.as_str() {
            "--mac" => mac = Some(normalise_mac(&value()?)?),
            "--secret" => secret = Some(value()?),
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
    let secret = secret.ok_or_else(|| anyhow!("--secret is required"))?;
    if secret.is_empty() {
        bail!("--secret must not be empty");
    }
    Ok(Args {
        mac,
        secret,
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

    aws_sdk_dynamodb::Client::new(&config)
        .put_item()
        .table_name(&args.table)
        .item("mac_address", AttributeValue::S(args.mac.clone()))
        .item("secret", AttributeValue::S(args.secret.clone()))
        .item("allowed", AttributeValue::Bool(args.allowed))
        .item("provisioned", AttributeValue::Bool(false))
        .send()
        .await
        .map_err(|e| anyhow!("{}", aws_sdk_dynamodb::error::DisplayErrorContext(e)))
        .with_context(|| format!("DynamoDB PutItem into {}", args.table))?;

    println!(
        "OK: {} added to table ({}). allowed={}",
        args.mac, args.table, args.allowed
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
                secret: "s".into(),
                table: "esp32-ztp-device-registry".into(),
                region: "eu-central-1".into(),
                allowed: true,
            }
        );
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
