#!/usr/bin/env bash
# Verify a downloaded, signed manifest using a public key obtained independently.
set -euo pipefail
if [[ $# != 3 ]]; then
  echo 'usage: verify-release.sh <SHA256SUMS-file> <minisign-public-key> <archive-file>' >&2
  exit 2
fi
manifest="$(realpath "$1")"
archive="$(realpath "$3")"
minisign -Vm "$manifest" -p "$2"
# Verify only the requested archive, rather than arbitrary paths supplied in a manifest.
name="$(basename "$archive")"
expected="$(awk -v name="$name" '$2 == name && $1 ~ /^[0-9a-f]+$/ {print $1}' "$manifest")"
[[ ${#expected} == 64 ]] || { echo 'missing/ambiguous archive checksum' >&2; exit 1; }
actual="$(sha256sum "$archive")"
[[ "${actual%% *}" == "$expected" ]] || { echo 'checksum mismatch' >&2; exit 1; }
printf 'signature and archive checksum verified: %s\n' "$name"
