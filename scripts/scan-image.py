#!/usr/bin/env python3
"""Fails if a firmware image carries a secret.

Images are public (platform plan, Phase 2): they may hold the Amazon root CA
and nothing else in PEM form. This looks for real PEM blocks (a BEGIN line
followed by base64), not for the marker strings mbedTLS's parser and the SDK's
checks contain anyway.

    scripts/scan-image.py firmware.bin [allowed.pem ...]

Fails on any private key block, and on any certificate block that is not one
of the allowed PEMs (default: certs/AmazonRootCA1.pem). Prints no image bytes.
"""

import re
import sys
from pathlib import Path

BLOCK = re.compile(
    rb"-----BEGIN ([A-Z0-9 ]+)-----\r?\n((?:[A-Za-z0-9+/=]{16,}\r?\n)+)-----END \1-----"
)


def body(pem: bytes) -> bytes:
    return re.sub(rb"\s", b"", pem)


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    image = Path(sys.argv[1]).read_bytes()
    allowed_files = sys.argv[2:] or ["certs/AmazonRootCA1.pem"]
    allowed = set()
    for f in allowed_files:
        for m in BLOCK.finditer(Path(f).read_bytes()):
            allowed.add(body(m.group(2)))

    problems = []
    for m in BLOCK.finditer(image):
        kind = m.group(1).decode()
        if "PRIVATE KEY" in kind:
            problems.append(f"a {kind} block at offset {m.start()}")
        elif body(m.group(2)) not in allowed:
            problems.append(f"an unexpected {kind} block at offset {m.start()}")
    if problems:
        for p in problems:
            print(f"::error::the image carries {p}")
        return 1
    print("image scan: no private keys, no certificates but the allowed CA")
    return 0


if __name__ == "__main__":
    sys.exit(main())
