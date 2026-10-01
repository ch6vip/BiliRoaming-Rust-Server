#!/usr/bin/env python3
"""Audit locked crates with OSV, including the explicitly verified local h2 backport."""
import argparse
import hashlib
import json
from pathlib import Path
import sys
import time
import tomllib
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parent.parent


def vendor_digest(directory):
    digest = hashlib.sha256()
    files = [directory / "Cargo.toml", *sorted((directory / "src").rglob("*"))]
    for file in files:
        if file.is_file():
            # Line endings may change on a Windows checkout; Rust source semantics do not.
            data = file.read_bytes().replace(b"\r\n", b"\n")
            digest.update(file.relative_to(directory).as_posix().encode() + b"\0" + data + b"\0")
    return digest.hexdigest()


def query_osv(queries):
    request = urllib.request.Request(
        "https://api.osv.dev/v1/querybatch",
        data=json.dumps({"queries": queries}).encode(),
        headers={"Content-Type": "application/json", "User-Agent": "BiliRoaming-dependency-audit"},
    )
    for attempt in range(3):
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                result = json.load(response)["results"]
            if len(result) != len(queries):
                raise ValueError("OSV returned an incomplete response")
            # Avoid silently skipping vulnerability pages if OSV introduces pagination here.
            if any(entry.get("next_page_token") for entry in result):
                raise ValueError("OSV returned paginated results; update the audit client")
            return result
        except (urllib.error.URLError, TimeoutError):
            if attempt == 2:
                raise
            time.sleep(attempt + 1)


def audit():
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))
    backport = json.loads((ROOT / "vendor/h2/backport.json").read_text(encoding="utf-8"))
    if manifest["patch"]["crates-io"]["h2"].get("path") != "vendor/h2":
        raise ValueError("Expected the documented local h2 override")
    vendor_manifest = tomllib.loads((ROOT / "vendor/h2/Cargo.toml").read_text(encoding="utf-8"))
    if (vendor_manifest["package"]["name"], vendor_manifest["package"]["version"]) != ("h2", "0.3.27"):
        raise ValueError("Unexpected h2 backport identity")
    if vendor_digest(ROOT / "vendor/h2") != backport["source_sha256"]:
        raise ValueError("h2 backport source changed: review and verify the patch before updating its digest")
    packages = []
    for package in lock["package"]:
        if package.get("source", "").startswith("registry+"):
            packages.append(package)
        elif package["name"] == "h2" and package["version"] == "0.3.27" and "source" not in package:
            packages.append(package)
        elif package["name"] != manifest["package"]["name"]:
            raise ValueError(f"Unrecognized non-registry dependency: {package['name']}")
    if not any(p["name"] == "h2" and "source" not in p for p in packages):
        raise ValueError("The h2 security backport is not present in Cargo.lock")
    results = query_osv([
        {"package": {"name": p["name"], "ecosystem": "crates.io"}, "version": p["version"]}
        for p in packages
    ])
    outstanding, remediated = [], []
    for package, result in zip(packages, results):
        for advisory in result.get("vulns", []):
            finding = {"package": package["name"], "version": package["version"], "id": advisory["id"]}
            if (package["name"] == "h2" and package["version"] == "0.3.27"
                    and "source" not in package and advisory["id"] in backport["advisories"]):
                finding["evidence"] = "vendor/h2/SECURITY-PATCH.md; tests/network_security.rs"
                remediated.append(finding)
            else:
                outstanding.append(finding)
    return {"packages_checked": len(packages), "unresolved": outstanding, "verified_backports": remediated}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    try:
        report = audit()
    except Exception as error:
        print(f"Dependency audit failed: {error}", file=sys.stderr)
        return 2
    output = json.dumps(report, indent=2, ensure_ascii=False)
    print(output)
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(output + "\n", encoding="utf-8")
    return 1 if report["unresolved"] else 0


if __name__ == "__main__":
    sys.exit(main())
