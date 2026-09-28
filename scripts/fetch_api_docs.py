#!/usr/bin/env python3
"""Fetch the official Super Market API reference and extract its embedded OpenAPI spec.

The docs page (https://sig.thesuper.market/api/v1/docs) is a Scalar reference with the full
OpenAPI 3.1 document inlined as the `content` of `Scalar.createApiReference(...)`. We pull it
out verbatim so the rest of the repo can diff it, lint against it, and never guess endpoints.

Snapshots land in brief/api/official/ (gitignored — they are the vendor's document, not ours).
A small manifest (hash + endpoint list) is printed so the daily summary can flag doc changes.

Usage:  python3 scripts/fetch_api_docs.py [--out brief/api/official] [--check]
Stdlib only, on purpose: this must run on a bare server before any venv exists.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import urllib.request
from datetime import UTC, datetime
from pathlib import Path

DOCS_URL = "https://sig.thesuper.market/api/v1/docs"
GUIDE_PAGES = [
    "https://sig.thesuper.market/docs",
    "https://sig.thesuper.market/docs/markets-and-trading",
    "https://sig.thesuper.market/docs/settlement-and-payouts",
    "https://sig.thesuper.market/docs/portfolio-and-leaderboards",
    "https://sig.thesuper.market/docs/tournaments-and-organizations",
    "https://sig.thesuper.market/docs/account-and-notifications",
    "https://sig.thesuper.market/docs/changelog",
    "https://predictionscup.com/rules/",
]
UA = (
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/128 Safari/537.36"
)


def fetch(url: str) -> str:
    if not url.startswith("https://"):
        raise ValueError(f"refusing non-https url: {url}")
    req = urllib.request.Request(url, headers={"User-Agent": UA, "Accept": "text/html,*/*"})  # noqa: S310
    with urllib.request.urlopen(req, timeout=30) as resp:  # noqa: S310 - https-only, checked above
        return resp.read().decode("utf-8", errors="replace")


def extract_openapi(page: str) -> dict:
    marker = "Scalar.createApiReference("
    start = page.index(marker)
    brace = page.index("{", start)
    config, _ = json.JSONDecoder().raw_decode(page[brace:])
    spec = config.get("content")
    if not isinstance(spec, dict) or "paths" not in spec:
        raise ValueError("docs page did not contain an inline OpenAPI document")
    return spec


def endpoint_list(spec: dict) -> list[str]:
    out = []
    for path, ops in spec["paths"].items():
        for method in ops:
            if method in {"get", "post", "put", "patch", "delete"}:
                out.append(f"{method.upper()} {path}")
    return sorted(out)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--out", default="brief/api/official")
    ap.add_argument("--check", action="store_true", help="exit 3 if the spec changed vs the last snapshot")
    args = ap.parse_args()

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    spec = extract_openapi(fetch(DOCS_URL))
    blob = json.dumps(spec, indent=2, sort_keys=True).encode()
    digest = hashlib.sha256(blob).hexdigest()

    manifest_path = out / "manifest.json"
    previous = json.loads(manifest_path.read_text()) if manifest_path.exists() else {}

    (out / "openapi.json").write_bytes(blob)
    for url in GUIDE_PAGES:
        name = url.rstrip("/").rsplit("/", 1)[-1] or "index"
        try:
            (out / f"guide_{name}.html").write_text(fetch(url))
        except OSError as exc:  # guide pages are nice-to-have
            print(f"warn: could not fetch {url}: {exc}", file=sys.stderr)

    endpoints = endpoint_list(spec)
    manifest = {
        "fetched_at": datetime.now(UTC).isoformat(timespec="seconds"),
        "source": DOCS_URL,
        "openapi_version": spec.get("openapi"),
        "api_version": spec.get("info", {}).get("version"),
        "sha256": digest,
        "endpoint_count": len(endpoints),
        "endpoints": endpoints,
    }
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")

    changed = previous.get("sha256") not in (None, digest)
    added = sorted(set(endpoints) - set(previous.get("endpoints", endpoints)))
    removed = sorted(set(previous.get("endpoints", endpoints)) - set(endpoints))
    print(json.dumps({"sha256": digest, "changed": changed, "added": added, "removed": removed}))
    return 3 if (args.check and changed) else 0


if __name__ == "__main__":
    sys.exit(main())
