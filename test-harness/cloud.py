"""The cloud half of the harness: the two-plane configuration the firmware
expects, plus the observability needed to assert on what the device did.

Two planes, because a 250-tag list does not fit in AWS IoT's 8 KB shadow
(docs/OPCUA_CLIENT_REQUIREMENTS.md §3):

  * control plane - the `opcua` NAMED shadow carries the small stuff and, in
    `cfg`, a *pointer* to the tag list: version, count, SHA-256, topic.
  * data plane    - the tag bundle itself, published RETAINED on that topic so
    a device that reboots gets it without any request/response dance.

The device applies a bundle only when both `v` and `sha256` match what the
shadow asked for, so these two writes cannot silently desynchronise. That is
also why `build_bundle` is careful to be byte-deterministic: the digest is
computed over exactly the bytes that get published.
"""

from __future__ import annotations

import hashlib
import json
import time
from dataclasses import dataclass
from typing import Any, Optional

import boto3

from tags import NAMESPACE_INDEX, NAMESPACE_URI, Tag, bundle_groups

SHADOW_NAME = "opcua"

#: Must match `gateway_core::MAX_BUNDLE_BYTES`; exceeding it is rejected on the
#: device before parsing, so the harness checks locally rather than shipping a
#: payload that can only fail.
MAX_BUNDLE_BYTES = 10 * 1024

#: CloudWatch log group fed by the `dt/+/opcua` IoT rule (iot-platform-infra).
TELEMETRY_LOG_GROUP = "/esp32-ztp/opcua-telemetry"


def _compact(obj: Any) -> bytes:
    """Serialises without incidental whitespace.

    The SHA-256 in the shadow is over these exact bytes, so the separators are
    part of the contract, not a formatting preference.
    """
    return json.dumps(obj, separators=(",", ":"), sort_keys=False).encode()


@dataclass
class Bundle:
    version: int
    payload: bytes
    sha256: str
    count: int
    topic: str

    @property
    def as_json(self) -> dict:
        return json.loads(self.payload)


def build_bundle(thing: str, version: int, tags: list[Tag]) -> Bundle:
    """Builds the grouped wire-form tag bundle and its digest."""
    doc = {"v": version, "g": bundle_groups(tags)}
    payload = _compact(doc)
    if len(payload) > MAX_BUNDLE_BYTES:
        raise ValueError(
            f"bundle is {len(payload)} B, firmware cap is {MAX_BUNDLE_BYTES} B"
        )
    return Bundle(
        version=version,
        payload=payload,
        sha256=hashlib.sha256(payload).hexdigest(),
        count=len(tags),
        topic=f"cmd/{thing}/opcua/tags/v{version}",
    )


def desired_document(
    thing: str,
    bundle: Bundle,
    endpoint: str,
    *,
    enabled: bool = True,
    ns: int = NAMESPACE_INDEX,
    ns_uri: Optional[str] = NAMESPACE_URI,
    sec_policy: str = "None",
    sec_mode: str = "None",
    publish_ms: int = 1000,
    batch_max_items: int = 100,
    batch_max_bytes: int = 16384,
    batch_max_age_ms: int = 2000,
    sha256_override: Optional[str] = None,
) -> dict:
    """Builds `state.desired` for the `opcua` named shadow.

    The overrides exist so the negative phases can ship a document that is
    well-formed but must be REFUSED (a non-`None` security policy, a digest
    that does not match the bundle) — checking that the device rejects those
    loudly matters more than checking that it accepts good ones.
    """
    instance = {
        "endpoint": endpoint,
        "ns": ns,
        "id_type": "s",
        "sec_mode": sec_mode,
        "sec_policy": sec_policy,
        "session_timeout_ms": 60000,
        "keepalive_ms": 10000,
        "publish_ms": publish_ms,
    }
    if ns_uri is not None:
        instance["ns_uri"] = ns_uri

    return {
        "enabled": enabled,
        "instance": instance,
        "telemetry": {
            "topic": f"dt/{thing}/opcua",
            "qos": 1,
            "batch_max_items": batch_max_items,
            "batch_max_bytes": batch_max_bytes,
            "batch_max_age_ms": batch_max_age_ms,
        },
        "cfg": {
            "v": bundle.version,
            "n": bundle.count,
            "sha256": sha256_override or bundle.sha256,
            "topic": bundle.topic,
        },
    }


class Cloud:
    """Thin wrapper over the two AWS IoT APIs the harness needs."""

    def __init__(self, thing: str, region: str = "eu-central-1") -> None:
        self.thing = thing
        self.data = boto3.client("iot-data", region_name=region)
        self.logs = boto3.client("logs", region_name=region)

    # -- data plane --------------------------------------------------------

    def publish_bundle(self, bundle: Bundle) -> None:
        """Publishes the tag bundle RETAINED.

        Retained is the whole point: the device subscribes to this topic only
        after it has read the shadow, which happens long after the publish.
        A non-retained message would simply never be seen.
        """
        self.data.publish(
            topic=bundle.topic, qos=1, retain=True, payload=bundle.payload
        )

    def clear_retained(self, topic: str) -> None:
        """Clears a retained message (an empty retained payload deletes it)."""
        self.data.publish(topic=topic, qos=1, retain=True, payload=b"")

    # -- control plane -----------------------------------------------------

    def update_desired(self, desired: dict) -> dict:
        payload = _compact({"state": {"desired": desired}})
        resp = self.data.update_thing_shadow(
            thingName=self.thing, shadowName=SHADOW_NAME, payload=payload
        )
        return json.loads(resp["payload"].read())

    def get_shadow(self) -> Optional[dict]:
        try:
            resp = self.data.get_thing_shadow(
                thingName=self.thing, shadowName=SHADOW_NAME
            )
        except self.data.exceptions.ResourceNotFoundException:
            return None
        return json.loads(resp["payload"].read())

    def reported(self) -> dict:
        doc = self.get_shadow() or {}
        return doc.get("state", {}).get("reported", {}) or {}

    def delete_shadow(self) -> None:
        try:
            self.data.delete_thing_shadow(
                thingName=self.thing, shadowName=SHADOW_NAME
            )
        except self.data.exceptions.ResourceNotFoundException:
            pass

    def wait_for_reported(
        self,
        predicate,
        timeout_s: float,
        poll_s: float = 3.0,
        on_poll=None,
    ) -> tuple[bool, dict]:
        """Polls `reported` until `predicate(reported)` holds or time runs out.

        Polling rather than subscribing keeps the harness free of a device
        certificate: `GetThingShadow` is an IAM-authorised HTTP call, so the
        test needs no MQTT identity of its own.
        """
        deadline = time.monotonic() + timeout_s
        last: dict = {}
        while time.monotonic() < deadline:
            last = self.reported()
            if on_poll:
                on_poll(last)
            if predicate(last):
                return True, last
            time.sleep(poll_s)
        return False, last

    # -- telemetry observation --------------------------------------------

    def telemetry_since(self, start_ms: int, limit: int = 2000) -> list[dict]:
        """Reads batches the `dt/+/opcua` IoT rule wrote to CloudWatch Logs.

        Returns the decoded batch documents (`{t, v, d:[...]}`), oldest first.
        """
        events: list[dict] = []
        kwargs = {
            "logGroupName": TELEMETRY_LOG_GROUP,
            "startTime": start_ms,
            "limit": min(limit, 10000),
        }
        try:
            pages = self.logs.get_paginator("filter_log_events").paginate(**kwargs)
            for page in pages:
                for event in page.get("events", []):
                    try:
                        events.append(json.loads(event["message"]))
                    except json.JSONDecodeError:
                        continue
                if len(events) >= limit:
                    break
        except self.logs.exceptions.ResourceNotFoundException:
            return []
        return events

    def wait_for_telemetry(
        self, start_ms: int, minimum: int, timeout_s: float, poll_s: float = 5.0
    ) -> list[dict]:
        """Waits until at least `minimum` batches have landed in CloudWatch.

        The IoT rule -> CloudWatch hop is asynchronous and typically lags a few
        seconds, so a bare `telemetry_since` right after a publish under-reports.
        """
        deadline = time.monotonic() + timeout_s
        batches: list[dict] = []
        while time.monotonic() < deadline:
            batches = self.telemetry_since(start_ms)
            if len(batches) >= minimum:
                return batches
            time.sleep(poll_s)
        return batches


def rows_by_address(batches: list[dict]) -> dict[str, list]:
    """Flattens telemetry batches into `{address: [row, ...]}`.

    A row is `[address, source_ts_ms, value]`, with an optional 4th element
    carrying the StatusCode when it is not Good (§4.3).
    """
    out: dict[str, list] = {}
    for batch in batches:
        for row in batch.get("d", []):
            if not row:
                continue
            out.setdefault(row[0], []).append(row)
    return out
