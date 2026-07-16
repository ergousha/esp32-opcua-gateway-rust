#!/usr/bin/env python3
"""Adds/updates a device in the DynamoDB device registry.

During Fleet Provisioning, the MAC + secret sent by the device using the claim cert is validated
against this table by the Lambda pre-provisioning hook. Provisioning will be REJECTED unless the
device is added here.

Usage:
    python3 seed_device.py --mac AA:BB:CC:DD:EE:FF --secret change-me-shared-secret

The device logs its MAC address to the serial port on first boot ("Provisioning starting. MAC=...").

Required: boto3 (pip install boto3) and valid AWS credentials.
"""
import argparse
import sys

import boto3
from botocore.exceptions import ClientError, NoCredentialsError

DEFAULT_TABLE = "esp32-ztp-device-registry"  # terraform project_name-device-registry


def main() -> int:
    p = argparse.ArgumentParser(description="Add an ESP32 device to the DynamoDB registry")
    p.add_argument("--mac", required=True, help="Device MAC, e.g. AA:BB:CC:DD:EE:FF")
    p.add_argument("--secret", required=True, help="Shared secret (same as firmware)")
    p.add_argument("--table", default=DEFAULT_TABLE, help=f"DynamoDB table name (default: {DEFAULT_TABLE})")
    p.add_argument("--region", default="eu-central-1", help="AWS region")
    p.add_argument("--allowed", default="true", choices=["true", "false"], help="Allow provisioning")
    args = p.parse_args()

    mac = args.mac.upper()
    ddb = boto3.resource("dynamodb", region_name=args.region)
    table = ddb.Table(args.table)

    item = {
        "mac_address": mac,
        "secret": args.secret,
        "allowed": args.allowed == "true",
        "provisioned": False,
    }

    try:
        table.put_item(Item=item)
    except NoCredentialsError:
        print("ERROR: AWS credentials not found. Please run `aws configure --profile esp32-ztp` "
              "and set `export AWS_PROFILE=esp32-ztp`.", file=sys.stderr)
        return 1
    except ClientError as exc:
        print(f"ERROR: DynamoDB put_item failed: {exc}", file=sys.stderr)
        return 1

    print(f"OK: {mac} added to table ({args.table}). allowed={item['allowed']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
