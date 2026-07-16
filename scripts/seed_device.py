#!/usr/bin/env python3
"""DynamoDB cihaz kayit registry'sine bir cihaz ekler/gunceller.

Fleet Provisioning'de cihazin claim ile gonderdigi MAC + secret, Lambda
pre-provisioning hook tarafindan bu tabloya karsi dogrulanir. Cihazi buraya
eklemeden provisioning REDDEDILIR.

Kullanim:
    python3 seed_device.py --mac AA:BB:CC:DD:EE:FF --secret change-me-shared-secret

MAC'i cihaz ilk bootta seri porta loglar ("Provisioning basliyor. MAC=...").

Gerekli: boto3  (pip install boto3) ve gecerli AWS kimlik bilgileri.
"""
import argparse
import sys

import boto3
from botocore.exceptions import ClientError, NoCredentialsError

DEFAULT_TABLE = "esp32-ztp-device-registry"  # terraform project_name-device-registry


def main() -> int:
    p = argparse.ArgumentParser(description="ESP32 cihazini DynamoDB registry'ye ekle")
    p.add_argument("--mac", required=True, help="Cihaz MAC'i, or. AA:BB:CC:DD:EE:FF")
    p.add_argument("--secret", required=True, help="Paylasilan secret (firmware ile ayni)")
    p.add_argument("--table", default=DEFAULT_TABLE, help=f"DynamoDB tablo adi (varsayilan: {DEFAULT_TABLE})")
    p.add_argument("--region", default="eu-central-1", help="AWS bolgesi")
    p.add_argument("--allowed", default="true", choices=["true", "false"], help="Provisioning'e izin ver")
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
        print("HATA: AWS kimlik bilgisi yok. `aws configure --profile esp32-ztp` calistirin "
              "ve `export AWS_PROFILE=esp32-ztp` yapin.", file=sys.stderr)
        return 1
    except ClientError as exc:
        print(f"HATA: DynamoDB put_item basarisiz: {exc}", file=sys.stderr)
        return 1

    print(f"OK: {mac} tabloya eklendi ({args.table}). allowed={item['allowed']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
