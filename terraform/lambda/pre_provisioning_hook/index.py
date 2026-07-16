"""
AWS IoT Fleet Provisioning - Pre-Provisioning Hook.

Cihaz claim sertifikasiyla baglanip RegisterThing istegi gonderdiginde,
IoT Core sertifika/thing OLUSTURMADAN ONCE bu Lambda'yi cagirir.

Gorevi: cihazin gonderdigi MAC + secret'i DynamoDB'deki kayitla dogrulamak.
- Kayit yoksa / secret uyusmuyor / allowed=false  -> provisioning REDDEDILIR.
- Gecerliyse -> allowProvisioning=true doner ve thing attribute'lari set edilir.

Event ornegi (payloadVersion 2020-04-01):
{
  "claimCertificateId": "...",
  "certificateId": "...",
  "parameters": {
    "SerialNumber": "AABBCCDDEEFF",
    "MacAddress":   "AA:BB:CC:DD:EE:FF",
    "Secret":       "change-me-shared-secret"
  },
  "templateArn":  "arn:aws:iot:...:provisioningtemplate/...",
  "templateName": "esp32-s3-fleet-template"
}
"""

import logging
import os

import boto3
from botocore.exceptions import ClientError

logger = logging.getLogger()
logger.setLevel(logging.INFO)

TABLE_NAME = os.environ["DEVICE_TABLE"]
_dynamodb = boto3.resource("dynamodb")
_table = _dynamodb.Table(TABLE_NAME)

# IoT Core hook cevabini ~5sn icinde bekler; deny durumunda net loglayin.
DENY = {"allowProvisioning": False}


def handler(event, context):
    params = event.get("parameters", {}) or {}
    mac = params.get("MacAddress")
    secret = params.get("Secret")
    serial = params.get("SerialNumber")

    logger.info("Pre-provisioning istegi: mac=%s serial=%s template=%s",
                mac, serial, event.get("templateName"))

    if not mac or not secret:
        logger.warning("REDDEDILDI: MacAddress veya Secret parametresi eksik.")
        return DENY

    try:
        resp = _table.get_item(Key={"mac_address": mac}, ConsistentRead=True)
    except ClientError as exc:
        logger.error("DynamoDB get_item hatasi: %s", exc)
        return DENY

    item = resp.get("Item")
    if not item:
        logger.warning("REDDEDILDI: %s kaydi DynamoDB'de yok.", mac)
        return DENY

    if not item.get("allowed", False):
        logger.warning("REDDEDILDI: %s allowed=false.", mac)
        return DENY

    # Sabit zamanli karsilastirma (secret kisa; timing riski dusuk ama iyi pratik).
    if not _constant_time_eq(str(item.get("secret", "")), str(secret)):
        logger.warning("REDDEDILDI: %s secret uyusmuyor.", mac)
        return DENY

    # (Opsiyonel) tek seferlik provisioning: zaten provisioned ise reddet.
    # PoC'de yeniden provisioning'e izin vermek icin bu blok yorumda.
    # if item.get("provisioned", False):
    #     logger.warning("REDDEDILDI: %s zaten provisioned.", mac)
    #     return DENY

    # Kaydi "provisioned" olarak isaretle (best-effort; basarisiz olsa da izin ver).
    try:
        _table.update_item(
            Key={"mac_address": mac},
            UpdateExpression="SET provisioned = :t, last_provisioned_serial = :s",
            ExpressionAttributeValues={":t": True, ":s": serial or mac},
        )
    except ClientError as exc:
        logger.error("provisioned flag guncellenemedi (devam ediliyor): %s", exc)

    logger.info("ONAYLANDI: %s icin provisioning'e izin verildi.", mac)
    return {
        "allowProvisioning": True,
        # Template'e ekstra/override parametre gecebilirsiniz:
        "parameterOverrides": {
            "MacAddress": mac,
        },
    }


def _constant_time_eq(a: str, b: str) -> bool:
    if len(a) != len(b):
        return False
    result = 0
    for x, y in zip(a, b):
        result |= ord(x) ^ ord(y)
    return result == 0
