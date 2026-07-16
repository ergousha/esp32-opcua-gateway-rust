"""
AWS IoT Fleet Provisioning - Pre-Provisioning Hook.

When a device connects with the claim certificate and sends a RegisterThing request,
IoT Core invokes this Lambda BEFORE creating the certificate and thing.

Its job is to validate the MAC + secret sent by the device against the registry record in DynamoDB.
- If there is no record / secret mismatch / allowed=false -> provisioning is REJECTED.
- If valid -> returns allowProvisioning=true and sets the thing attributes.

Event example (payloadVersion 2020-04-01):
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

# IoT Core expects the hook response within ~5 seconds; make sure to log deny conditions clearly.
DENY = {"allowProvisioning": False}


def handler(event, context):
    params = event.get("parameters", {}) or {}
    mac = params.get("MacAddress")
    secret = params.get("Secret")
    serial = params.get("SerialNumber")

    logger.info("Pre-provisioning request: mac=%s serial=%s template=%s",
                mac, serial, event.get("templateName"))

    if not mac or not secret:
        logger.warning("REJECTED: MacAddress or Secret parameter missing.")
        return DENY

    try:
        resp = _table.get_item(Key={"mac_address": mac}, ConsistentRead=True)
    except ClientError as exc:
        logger.error("DynamoDB get_item error: %s", exc)
        return DENY

    item = resp.get("Item")
    if not item:
        logger.warning("REJECTED: %s record does not exist in DynamoDB.", mac)
        return DENY

    if not item.get("allowed", False):
        logger.warning("REJECTED: %s allowed=false.", mac)
        return DENY

    # Constant time comparison (secret is short; timing risk is low but it is good practice).
    if not _constant_time_eq(str(item.get("secret", "")), str(secret)):
        logger.warning("REJECTED: %s secret mismatch.", mac)
        return DENY

    # (Optional) one-time provisioning: reject if already provisioned.
    # This block is commented out in PoC to allow re-provisioning.
    # if item.get("provisioned", False):
    #     logger.warning("REJECTED: %s already provisioned.", mac)
    #     return DENY

    # Mark the record as "provisioned" (best-effort; allow even if this update fails).
    try:
        _table.update_item(
            Key={"mac_address": mac},
            UpdateExpression="SET provisioned = :t, last_provisioned_serial = :s",
            ExpressionAttributeValues={":t": True, ":s": serial or mac},
        )
    except ClientError as exc:
        logger.error("Failed to update provisioned flag (continuing anyway): %s", exc)

    logger.info("APPROVED: provisioning allowed for %s.", mac)
    return {
        "allowProvisioning": True,
        # You can pass extra/override parameters to the template:
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
