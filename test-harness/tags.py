"""The tag catalogue: the single source of truth for the whole scenario.

Both sides of the test read this module, which is the point. The OPC UA server
(`opcua_test_server.py`) creates a node for every entry that has a `variant`,
and the cloud publisher (`cloud.py`) builds the tag bundle from the same list.
If the two were maintained separately, a typo in an address would look exactly
like a gateway bug.

Each tag also declares what the *telemetry payload* should look like once the
firmware has encoded it (`expects`), so `run_scenario.py` can assert on the
actual bytes that reached AWS rather than merely counting them.
"""

from dataclasses import dataclass, field
from typing import Any, Callable, Optional

# Namespace the test server registers. The gateway resolves this URI against the
# server's NamespaceArray; the numeric index is only a fallback.
NAMESPACE_URI = "urn:ergousha:opcua-test"

# Index the URI is expected to land on: 0 is the OPC UA base namespace, 1 is the
# server's own application URI, so a single registered namespace becomes 2.
NAMESPACE_INDEX = 2

# Scan rates in use. Each distinct rate becomes one OPC UA subscription on the
# device, so keeping this list short is itself part of the design under test.
FAST_MS = 1000
SLOW_MS = 5000


@dataclass
class Tag:
    """One tag in the scenario.

    `address` is the bare SCADA-style address; the gateway turns it into
    `ns=<n>;s=<address>`. `variant` is the `ua.VariantType` name the server
    creates the node with — `None` means the node is deliberately *not* created,
    which is how the missing-tag path gets exercised.
    """

    address: str
    scan_rate_ms: int
    variant: Optional[str]
    initial: Any = None
    #: Called every server tick with the tick counter; returns the new value.
    #: `None` means the value never changes.
    step: Optional[Callable[[int], Any]] = None
    #: When set, the server publishes this value with a Bad StatusCode.
    bad_status: Optional[str] = None
    #: Human-readable description of what this tag proves.
    proves: str = ""
    #: JSON type the firmware is expected to emit: "number", "bool", "string",
    #: "array", or a tagged-object tag such as "i64" / "u64" / "b64" / "guid".
    expects: str = "number"


import math  # noqa: E402  (kept below the dataclass for readability)


def _sine(tick: int) -> float:
    return round(20.0 + 5.0 * math.sin(tick / 4.0), 3)


def _ramp(tick: int) -> float:
    # Deliberately a value that is NOT exact in binary floating point, so the
    # f32 -> f64 widening in `variant.rs::shortest_f32` is actually observable:
    # a naive `f as f64` would surface 1.100000023841858 instead of 1.1.
    return round(1.0 + (tick % 10) * 0.1, 1)


def _toggle(tick: int) -> bool:
    return (tick // 4) % 2 == 0


def _state(tick: int) -> str:
    return ["RUN", "IDLE", "FAULT"][(tick // 6) % 3]


# Base for the 64-bit counters: above 2^53, where an IEEE-754 double stops being
# able to represent every integer. A gateway that routes these through a float
# corrupts them silently, so the encoder is expected to emit a tagged string.
ABOVE_2_53 = 9_007_199_254_740_993  # 2^53 + 1


TAGS: list[Tag] = [
    # ---- fast group (1 s) -------------------------------------------------
    Tag(
        address="Line1.Temp",
        scan_rate_ms=FAST_MS,
        variant="Double",
        initial=20.0,
        step=_sine,
        proves="plain Double -> bare JSON number",
        expects="number",
    ),
    Tag(
        address="Line1.Pressure",
        scan_rate_ms=FAST_MS,
        variant="Float",
        initial=1.0,
        step=_ramp,
        proves="Float widened via its shortest decimal form, not `as f64`",
        expects="number",
    ),
    Tag(
        address="Line1.Running",
        scan_rate_ms=FAST_MS,
        variant="Boolean",
        initial=True,
        step=_toggle,
        proves="Boolean -> JSON true/false",
        expects="bool",
    ),
    Tag(
        address="Line1.State",
        scan_rate_ms=FAST_MS,
        variant="String",
        initial="RUN",
        step=_state,
        proves="String -> JSON string",
        expects="string",
    ),
    Tag(
        address="Line1.Counter",
        scan_rate_ms=FAST_MS,
        variant="UInt32",
        initial=0,
        step=lambda t: t,
        proves="UInt32 -> bare JSON number (below 2^53)",
        expects="number",
    ),
    Tag(
        address="Line1.BigCounter",
        scan_rate_ms=FAST_MS,
        variant="Int64",
        initial=ABOVE_2_53,
        step=lambda t: ABOVE_2_53 + t,
        proves='Int64 past 2^53 -> {"$t":"i64"} tagged string, losslessly',
        expects="i64",
    ),
    Tag(
        address="Line1.Serial",
        scan_rate_ms=FAST_MS,
        variant="UInt64",
        initial=ABOVE_2_53 + 7,
        step=lambda t: ABOVE_2_53 + 7 + t,
        proves='UInt64 past 2^53 -> {"$t":"u64"} tagged string',
        expects="u64",
    ),
    Tag(
        address="Line1.Blob",
        scan_rate_ms=FAST_MS,
        variant="ByteString",
        initial=b"\x00\x01\x02\xfe\xff",
        step=lambda t: bytes([t % 256, 1, 2, 254, 255]),
        proves='ByteString -> {"$t":"b64"} base64',
        expects="b64",
    ),
    Tag(
        address="Line1.BatchId",
        scan_rate_ms=FAST_MS,
        variant="Guid",
        initial="72962b91-fa75-4ae6-8d28-b404dc7daf63",
        step=None,
        proves='Guid -> {"$t":"guid"}',
        expects="guid",
    ),
    Tag(
        address="Line1.Profile",
        scan_rate_ms=FAST_MS,
        variant="DoubleArray",
        initial=[1.0, 2.0, 3.0, 4.0, 5.0],
        step=lambda t: [float(t + i) for i in range(5)],
        proves="array Variant -> JSON array",
        expects="array",
    ),
    Tag(
        address="Line1.Static",
        scan_rate_ms=FAST_MS,
        variant="Double",
        initial=42.0,
        step=None,
        proves="a never-changing tag reports once, not every second "
        "(report-by-exception, i.e. the subscription is not polling)",
        expects="number",
    ),
    Tag(
        address="Line1.Faulty",
        scan_rate_ms=FAST_MS,
        variant="Double",
        initial=0.0,
        step=None,
        bad_status="BadDeviceFailure",
        proves="a Bad StatusCode travels as the optional 4th row element",
        expects="number",
    ),
    # ---- slow group (5 s) -------------------------------------------------
    Tag(
        address="Line2.Level",
        scan_rate_ms=SLOW_MS,
        variant="Double",
        initial=50.0,
        step=lambda t: round(50.0 + (t % 20), 2),
        proves="a second scan rate becomes a second subscription",
        expects="number",
    ),
    Tag(
        address="Line2.Mode",
        scan_rate_ms=SLOW_MS,
        variant="Int16",
        initial=1,
        step=lambda t: (t % 3) + 1,
        proves="Int16 -> bare JSON number",
        expects="number",
    ),
    # ---- deliberately absent ---------------------------------------------
    Tag(
        address="Line1.DoesNotExist",
        scan_rate_ms=FAST_MS,
        variant=None,
        proves="one unknown NodeId is reported as failed WITHOUT taking the "
        "other tags down (finding A4 in the requirements doc)",
        expects="absent",
    ),
]


#: Tags the server actually creates.
SERVER_TAGS = [t for t in TAGS if t.variant is not None]

#: Tags expected to appear in telemetry (created, and not permanently Bad-only).
REPORTING_TAGS = [t for t in SERVER_TAGS]

#: Addresses that must show up in `reported.failed_sample`.
MISSING_ADDRESSES = [t.address for t in TAGS if t.variant is None]


def _tagged(kind: str):
    """Predicate for a `{"$t": kind, "v": "..."}` tagged-object encoding."""
    return lambda v: isinstance(v, dict) and v.get("$t") == kind


#: Maps a tag's `expects` to a predicate over the JSON value the firmware
#: emitted. Keeping this next to the catalogue means a new tag declares its
#: expected encoding in one place and the scenario picks the assertion up for
#: free.
ENCODING_PREDICATES = {
    # `bool` must be tested before `number`: in Python `True` is an `int`.
    "bool": lambda v: isinstance(v, bool),
    "number": lambda v: isinstance(v, (int, float)) and not isinstance(v, bool),
    "string": lambda v: isinstance(v, str),
    "array": lambda v: isinstance(v, list),
    "i64": lambda v: _tagged("i64")(v) and abs(int(v["v"])) > 9_007_199_254_740_991,
    "u64": lambda v: _tagged("u64")(v) and int(v["v"]) > 9_007_199_254_740_991,
    "b64": _tagged("b64"),
    "guid": _tagged("guid"),
}


def bundle_groups(tags: list[Tag]) -> list[dict]:
    """Groups tags by scan rate into the bundle's `g` wire form.

    Order within a group is preserved so that re-publishing an unchanged tag set
    produces a byte-identical bundle and therefore the same SHA-256 — which is
    what makes the device's idempotency check meaningful.
    """
    by_rate: dict[int, list[str]] = {}
    for tag in tags:
        by_rate.setdefault(tag.scan_rate_ms, []).append(tag.address)
    return [{"r": rate, "a": addrs} for rate, addrs in sorted(by_rate.items())]
