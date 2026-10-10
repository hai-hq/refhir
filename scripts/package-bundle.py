#!/usr/bin/env python3
"""Build a package bundle for ReFHIR's default FHIR packages, and optionally upload it.

ReFHIR does not ship FHIR packages. On a first start it downloads the release defaults
(hl7.fhir.r4.core and hl7.fhir.uv.sdc for R4, see crates/fhir/src/package.rs) and every
package they depend on from `[conformance].package_registry`, which can be

  * a FHIR package registry (default https://packages.fhir.org),
  * any static host that serves <base>/<name>/<version> (a mirror, CloudFront, GCS, ...),
  * s3://bucket/prefix (AWS S3 or any S3-compatible store; keys in REFHIR_S3_ACCESS_KEY_ID /
    REFHIR_S3_SECRET_ACCESS_KEY, endpoint in REFHIR_S3_ENDPOINT).

This script makes that last two possible: it downloads the packages once from the public
registry, checks them, and writes them in the layout above, so you serve exactly the versions
you tested from storage you control.

    scripts/package-bundle.py                                   # defaults -> dist/packages
    scripts/package-bundle.py --s3 s3://my-bucket/fhir-packages # ... and upload with the aws CLI
    scripts/package-bundle.py --spec hl7.fhir.us.core@6.1.0     # add an implementation guide

The output is
    <out>/<name>/<version>     the package .tgz
    <out>/manifest.json        name, version, size and sha256 of every package

Packages are published byte for byte, with one deliberate exception. hl7.fhir.uv.sdc declares
hl7.fhir.r4.examples (a copy of the core definitions plus thousands of example resources) and
hl7.fhir.uv.xver-r5.r4 (R5 cross-version extensions) as dependencies, and installing them
roughly doubles a first start without adding anything the server serves. By default those two
names are removed from the `dependencies` of every mirrored package.json, so they are neither
mirrored nor installed. The rewritten tarball keeps everything else, is repacked reproducibly,
and the manifest records `source_sha256` (the published tarball) and `dropped_dependencies`.
Use --keep-all-dependencies for pristine copies, or --drop-dependency to drop more.

Only the Python standard library is needed; uploading needs the `aws` CLI (use
AWS_ENDPOINT_URL or --endpoint-url for MinIO, R2 and other S3-compatible stores).
"""

import argparse
import gzip
import hashlib
import io
import json
import re
import ssl
import subprocess
import sys
import tarfile
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_REGISTRY = "https://packages.fhir.org"
MAX_BYTES = 100 * 1024 * 1024  # the server refuses larger packages
# Dependencies that cost minutes on a first start and add nothing the server serves.
DEFAULT_DROPPED = ["hl7.fhir.r4.examples", "hl7.fhir.uv.xver-r5.r4"]


def default_specs():
    """The release defaults, read from the server source so the two never drift apart."""
    text = (ROOT / "crates/fhir/src/package.rs").read_text()
    specs = []
    for const in ("R4_CORE_PACKAGE", "SDC_PACKAGE"):
        match = re.search(rf'pub const {const}: &str = "([^"]+)";', text)
        if not match:
            sys.exit(f"cannot find {const} in crates/fhir/src/package.rs")
        specs.append(match.group(1))
    return specs


def parse_spec(spec):
    name, sep, version = spec.partition("@")
    if not sep or not name or not version:
        sys.exit(f"{spec}: expected name@version")
    return name, version


def download(registry, name, version):
    url = f"{registry.rstrip('/')}/{name}/{version}"
    last = None
    for attempt in range(3):
        try:
            request = urllib.request.Request(url, headers={"User-Agent": "refhir-package-bundle"})
            with urllib.request.urlopen(request, timeout=120) as response:
                data = response.read(MAX_BYTES + 1)
            if len(data) > MAX_BYTES:
                sys.exit(f"{name}@{version} is larger than {MAX_BYTES} bytes")
            return data
        except urllib.error.HTTPError as err:
            if 400 <= err.code < 500:
                sys.exit(f"{url}: HTTP {err.code}")
            last = err
        except urllib.error.URLError as err:
            if isinstance(err.reason, ssl.SSLCertVerificationError):
                sys.exit(
                    f"{url}: {err.reason}\n"
                    "Python has no CA certificates to check the server with. Point SSL_CERT_FILE at "
                    "your CA bundle (e.g. /etc/ssl/cert.pem), or on macOS run Python's "
                    "'Install Certificates.command'."
                )
            last = err
        except TimeoutError as err:
            last = err
        time.sleep(2 * (attempt + 1))
    sys.exit(f"{url}: {last}")


def manifest_of(name, version, data):
    """The package.json inside the tarball, after checking it is the package we asked for."""
    try:
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
            member = archive.extractfile("package/package.json")
            manifest = json.load(member)
    except (tarfile.TarError, OSError, KeyError, ValueError, gzip.BadGzipFile) as err:
        sys.exit(f"{name}@{version} is not a FHIR package: {err}")
    if manifest.get("name") != name or manifest.get("version") != version:
        sys.exit(
            f"asked for {name}@{version} but got {manifest.get('name')}@{manifest.get('version')}"
        )
    return manifest


def without_dependencies(data, dropped):
    """The tarball with `dropped` removed from package.json dependencies.

    Returns (data, names removed). Unchanged bytes when nothing needs removing. The repack is
    reproducible: same input, same output bytes.
    """
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as source:
        manifest = json.load(source.extractfile("package/package.json"))
        removed = [name for name in (manifest.get("dependencies") or {}) if name in dropped]
        if not removed:
            return data, []
        for name in removed:
            del manifest["dependencies"][name]
        if not manifest["dependencies"]:
            del manifest["dependencies"]
        edited = (json.dumps(manifest, indent=2) + "\n").encode()
        raw = io.BytesIO()
        with gzip.GzipFile(fileobj=raw, mode="wb", mtime=0) as zipped:
            with tarfile.open(fileobj=zipped, mode="w", format=tarfile.GNU_FORMAT) as target:
                for member in source.getmembers():
                    if member.name == "package/package.json":
                        member.size = len(edited)
                        target.addfile(member, io.BytesIO(edited))
                    elif member.isfile():
                        target.addfile(member, source.extractfile(member))
                    else:
                        target.addfile(member)
    return raw.getvalue(), removed


def exact_dependencies(manifest, owner):
    for name, version in (manifest.get("dependencies") or {}).items():
        if re.match(r"^[\^~<>*]", version) or version in ("latest", "") or " " in version:
            print(f"  skipping {name}@{version} (needed by {owner}): not an exact version")
            continue
        yield name, version


def previous_entries(out):
    """Manifest entries from an earlier run, by (name, version)."""
    try:
        listed = json.loads((out / "manifest.json").read_text())["packages"]
    except (OSError, ValueError, KeyError):
        return {}
    return {(item["name"], item["version"]): item for item in listed}


def build(specs, registry, out, with_dependencies, dropped):
    wanted = [parse_spec(spec) for spec in specs]
    previous = previous_entries(out)
    seen = {}
    while wanted:
        name, version = wanted.pop(0)
        if (name, version) in seen:
            continue
        target = out / name / version
        if target.is_file():
            data = target.read_bytes()
            print(f"  have {name}@{version}")
        else:
            print(f"  fetching {name}@{version}")
            data = download(registry, name, version)
        manifest_of(name, version, data)
        source_sha256 = hashlib.sha256(data).hexdigest()
        data, removed = without_dependencies(data, dropped)
        if removed:
            print(f"    dropping dependencies {', '.join(removed)}")
        manifest = manifest_of(name, version, data)
        target.parent.mkdir(parents=True, exist_ok=True)
        if not target.is_file() or target.read_bytes() != data:
            temporary = target.with_suffix(".part")
            temporary.write_bytes(data)
            temporary.replace(target)
        entry = {
            "name": name,
            "version": version,
            "bytes": len(data),
            "sha256": hashlib.sha256(data).hexdigest(),
        }
        if removed:
            entry["source_sha256"] = source_sha256
            entry["dropped_dependencies"] = removed
        else:
            # A file kept from an earlier run is already trimmed, so there is nothing to drop
            # now. Keep what that run recorded about the published tarball, if it is the same
            # file.
            earlier = previous.get((name, version), {})
            if earlier.get("sha256") == entry["sha256"]:
                for key in ("source_sha256", "dropped_dependencies"):
                    if key in earlier:
                        entry[key] = earlier[key]
        seen[(name, version)] = entry
        if with_dependencies:
            wanted.extend(exact_dependencies(manifest, f"{name}@{version}"))
    return sorted(seen.values(), key=lambda item: (item["name"], item["version"]))


def upload(out, destination, endpoint):
    base = ["aws", "s3"]
    extra = ["--endpoint-url", endpoint] if endpoint else []
    destination = destination.rstrip("/")
    try:
        subprocess.run(
            base
            + ["sync", str(out), destination, "--exclude", "manifest.json",
               "--content-type", "application/gzip"]
            + extra,
            check=True,
        )
        subprocess.run(
            base
            + ["cp", str(out / "manifest.json"), f"{destination}/manifest.json",
               "--content-type", "application/json"]
            + extra,
            check=True,
        )
    except FileNotFoundError:
        sys.exit("the aws CLI is needed to upload; install it, or copy the output directory yourself")
    except subprocess.CalledProcessError as err:
        sys.exit(f"upload failed: {err}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--spec", action="append", default=[], metavar="NAME@VERSION",
                        help="add a package (repeatable); the release defaults are always included")
    parser.add_argument("--no-defaults", action="store_true", help="only the --spec packages")
    parser.add_argument("--registry", default=DEFAULT_REGISTRY, help=f"source (default {DEFAULT_REGISTRY})")
    parser.add_argument("--out", default="dist/packages", help="output directory (default dist/packages)")
    parser.add_argument("--no-deps", action="store_true", help="do not follow package dependencies")
    parser.add_argument("--drop-dependency", action="append", default=[], metavar="NAME",
                        help="also remove NAME from every package's dependencies (repeatable)")
    parser.add_argument("--keep-all-dependencies", action="store_true",
                        help=f"mirror packages unchanged; do not drop {', '.join(DEFAULT_DROPPED)}")
    parser.add_argument("--s3", metavar="s3://BUCKET/PREFIX", help="upload the result with the aws CLI")
    parser.add_argument("--endpoint-url", help="S3-compatible endpoint for the upload (MinIO, R2, ...)")
    args = parser.parse_args()

    specs = ([] if args.no_defaults else default_specs()) + args.spec
    if not specs:
        parser.error("nothing to do: no defaults and no --spec")
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)

    print(f"building {len(specs)} package(s) from {args.registry} into {out}")
    dropped = set(args.drop_dependency)
    if not args.keep_all_dependencies:
        dropped.update(DEFAULT_DROPPED)
    packages = build(specs, args.registry, out, not args.no_deps, dropped)
    manifest = {
        "generated": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "registry": args.registry,
        "packages": packages,
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    total = sum(item["bytes"] for item in packages)
    print(f"{len(packages)} packages, {total / 1e6:.1f} MB, manifest at {out / 'manifest.json'}")

    if args.s3:
        print(f"uploading to {args.s3}")
        upload(out, args.s3, args.endpoint_url)
        location = args.s3
    else:
        location = "s3://BUCKET/PREFIX (or any URL that serves <base>/<name>/<version>)"
    print(f"\nPoint ReFHIR at it:\n  [conformance]\n  package_registry = \"{location}\"")


if __name__ == "__main__":
    main()
