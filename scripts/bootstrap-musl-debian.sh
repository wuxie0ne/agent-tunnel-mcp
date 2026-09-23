#!/usr/bin/env bash
# Optional, unprivileged build helper for Debian/Ubuntu x86_64 hosts.
# Downloads from the host's configured APT sources, then extracts ONLY under target/.
# It does not install packages into the host system; running it requires network access.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -m)" == x86_64 ]] || { echo 'This helper is only for x86_64 Debian/Ubuntu hosts.' >&2; exit 2; }
for command in apt-get apt-cache dpkg-deb gcc python3; do command -v "$command" >/dev/null; done
root="$PWD/target/musl-toolchain"
mkdir -p "$root/debs" "$root/root" "$root/bin"
version="${MUSL_DEB_VERSION:-$(apt-cache policy musl-tools | sed -n 's/^  Candidate: //p')}"
[[ -n "$version" && "$version" != '(none)' ]] || { echo 'No musl-tools candidate in configured APT indexes.' >&2; exit 1; }
(cd "$root/debs" && apt-get download "musl=$version" "musl-dev=$version" "musl-tools=$version")
for package in musl musl-dev musl-tools; do
  archive="$root/debs/${package}_${version}_amd64.deb"
  dpkg-deb -x "$archive" "$root/root"
done
python3 - "$root" <<'PY'
import pathlib, sys
root = pathlib.Path(sys.argv[1]).resolve()
spec = root / 'root/usr/lib/x86_64-linux-musl/musl-gcc.specs'
text = spec.read_text().replace('/usr/include/', str(root / 'root') + '/usr/include/').replace('/usr/lib/', str(root / 'root') + '/usr/lib/')
spec.write_text(text)
wrapper = root / 'bin/x86_64-linux-musl-gcc'
wrapper.write_text('#!/bin/sh\nexec gcc -specs="' + str(spec) + '" "$@"\n')
wrapper.chmod(0o755)
PY
printf '\nNo system packages were installed. Build with:\n  PATH="%s/bin:$PATH" scripts/package.sh [--force] x86_64-unknown-linux-musl\n' "$root"
printf 'Use --force only when you intend to replace same-name artifacts under dist/.\n'
