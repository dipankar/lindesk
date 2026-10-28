#!/usr/bin/env python3
"""Fail a release if any supported core/telemetry package or archive is missing."""
import argparse
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('directory', type=Path)
parser.add_argument('version')
args = parser.parse_args()
expected = []
for package in ('lindesk', 'lindesk-telemetry'):
    for arch in ('amd64', 'arm64'):
        expected.append(f'{package}_{args.version}-1_{arch}.deb')
    for arch in ('x86_64', 'aarch64'):
        expected.append(f'{package}-{args.version}-1.*.{arch}.rpm')
        expected.append(f'{package}-{args.version}-linux-{arch}.tar.zst')
    expected.append(f'{package}-{args.version}-1-x86_64.pkg.tar.zst')
matched = set()
for pattern in expected:
    files = list(args.directory.glob(pattern))
    if len(files) != 1 or not files[0].is_file() or files[0].stat().st_size == 0:
        parser.error(f'expected one nonempty asset for {pattern}, found {len(files)}')
    matched.add(files[0].name)
extra = {p.name for p in args.directory.iterdir() if p.name != 'SHA256SUMS' and not p.name.endswith('.src.rpm')} - matched
if extra:
    parser.error(f'unexpected release assets: {sorted(extra)}')
print(f'Verified all {len(matched)} release assets for {args.version}')
