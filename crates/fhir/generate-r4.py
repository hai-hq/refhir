#!/usr/bin/env python3
"""Download hl7.fhir.r4.core and write a fixture package.

The server installs a local directory. This script fetches the published R4
core package once and keeps StructureDefinition and SearchParameter resources.

A few SearchParameters are left out because the loader cannot install them:
the draft example uses base Resource and code _id with a different expression
than SearchParameter-Resource-id, and some extension parameters have no base.
"""

import json
import shutil
import subprocess
import tarfile
import tempfile
from pathlib import Path

PACKAGE_URL = "https://packages.fhir.org/hl7.fhir.r4.core/4.0.1"
SKIP_SEARCH_URLS = {"http://hl7.org/fhir/SearchParameter/example"}
ROOT = Path(__file__).resolve().parent
OUTPUT = ROOT / "fixtures" / "r4"


def main() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        archive_path = Path(tmp) / "hl7.fhir.r4.core-4.0.1.tgz"
        staging = Path(tmp) / "r4"
        staging.mkdir()
        download(PACKAGE_URL, archive_path)
        counts = extract(archive_path, staging)
        if OUTPUT.exists():
            shutil.rmtree(OUTPUT)
        shutil.move(str(staging), OUTPUT)
    print(
        f"wrote {OUTPUT} "
        f"({counts['StructureDefinition']} StructureDefinitions, "
        f"{counts['SearchParameter']} SearchParameters, "
        f"skipped {counts['skipped']} SearchParameters)"
    )


def download(url: str, path: Path) -> None:
    subprocess.run(["curl", "-fsSL", "-o", str(path), url], check=True)


def extract(archive_path: Path, staging: Path) -> dict[str, int]:
    counts = {"StructureDefinition": 0, "SearchParameter": 0, "skipped": 0}
    with tarfile.open(archive_path, "r:gz") as archive:
        manifest = None
        for member in archive.getmembers():
            name = Path(member.name).name
            if name == "package.json":
                manifest = json.load(archive.extractfile(member))
                continue
            if not name.endswith(".json"):
                continue
            resource_type = name.split("-", 1)[0]
            if resource_type not in ("StructureDefinition", "SearchParameter"):
                continue
            payload = archive.extractfile(member).read()
            if resource_type == "SearchParameter" and not keep_search(payload):
                counts["skipped"] += 1
                continue
            (staging / name).write_bytes(payload)
            counts[resource_type] += 1
    if manifest is None:
        raise SystemExit("package.json missing from hl7.fhir.r4.core")
    versions = manifest.get("fhir-version-list") or []
    if manifest.get("fhirVersion") is None and versions:
        manifest["fhirVersion"] = versions[0]
    (staging / "package.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return counts


def keep_search(payload: bytes) -> bool:
    parameter = json.loads(payload)
    if parameter.get("url") in SKIP_SEARCH_URLS:
        return False
    bases = parameter.get("base")
    if not isinstance(bases, list) or not bases:
        return False
    if not all(isinstance(base, str) and base for base in bases):
        return False
    return all(isinstance(parameter.get(field), str) and parameter[field] for field in ("url", "code", "type"))


if __name__ == "__main__":
    main()
