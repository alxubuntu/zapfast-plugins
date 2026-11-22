#!/usr/bin/env python3
"""Package a separately compiled example executable for one target."""
import argparse
import json
from pathlib import Path
import zipfile

parser = argparse.ArgumentParser()
parser.add_argument("--platform", required=True, help="e.g. linux-x86_64, windows-x86_64, macos-aarch64")
parser.add_argument("--executable", type=Path, required=True)
parser.add_argument("--output", type=Path, required=True)
args = parser.parse_args()
name = "bin/example-reply.exe" if args.platform.startswith("windows-") else "bin/example-reply"
manifest = {
    "id": "org.zapfast.example-reply-stage1",
    "version": "0.1.0",
    "api_version": 1,
    "executables": {args.platform: name},
}
with zipfile.ZipFile(args.output, "w", compression=zipfile.ZIP_DEFLATED) as archive:
    archive.writestr("manifest.json", json.dumps(manifest, indent=2))
    archive.write(args.executable, name)
