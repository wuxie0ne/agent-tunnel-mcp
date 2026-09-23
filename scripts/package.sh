#!/usr/bin/env bash
# Build full and connector-only distributions without overwriting dist artifacts by default.
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
  cat <<'USAGE'
usage: scripts/package.sh [--force] [target-triple]

Build full and connector-only tar.gz archives in dist/.
Existing same-name archives, checksum manifests, or signatures are never
replaced unless --force is supplied.
USAGE
}

force=0
target=""
while (($#)); do
  case "$1" in
    --force) force=1 ;;
    -h|--help) usage; exit 0 ;;
    --)
      shift
      (($# <= 1)) || { usage >&2; exit 2; }
      if (($#)); then
        [[ -z "$target" ]] || { usage >&2; exit 2; }
        target="$1"
      fi
      break
      ;;
    -*) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    *)
      [[ -z "$target" ]] || { usage >&2; exit 2; }
      target="$1"
      ;;
  esac
  shift
done

target="${target:-$(rustc -vV | sed -n 's/^host: //p')}"
[[ -n "$target" ]] || { echo 'could not determine target triple' >&2; exit 2; }
[[ "$target" =~ ^[A-Za-z0-9_][-A-Za-z0-9_.+]*$ ]] || {
  echo 'target triple contains unsupported characters' >&2
  exit 2
}
if [[ "$target" == x86_64-unknown-linux-musl && -x target/musl-toolchain/bin/x86_64-linux-musl-gcc ]]; then
  export PATH="$PWD/target/musl-toolchain/bin:$PATH"
fi
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)"
[[ -n "$version" ]] || { echo 'could not determine package version' >&2; exit 1; }
epoch="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || date +%s)}"
[[ "$epoch" =~ ^[0-9]+$ ]] || { echo 'SOURCE_DATE_EPOCH must be a non-negative integer' >&2; exit 2; }

dist="$PWD/dist"
mkdir -p "$dist"
full="agent-tunnel-${version}-${target}-full.tar.gz"
connector="agent-tunnel-${version}-${target}-connector.tar.gz"
manifest="SHA256SUMS-${target}"
signature="${manifest}.minisig"
outputs=("$full" "$connector" "$manifest" "$signature")

check_collisions() {
  local output
  for output in "${outputs[@]}"; do
    if [[ -e "$dist/$output" || -L "$dist/$output" ]]; then
      if ((force)); then
        continue
      fi
      echo "refusing to overwrite existing dist artifact: dist/$output (use --force to replace)" >&2
      return 1
    fi
  done
}
check_collisions

# Keep all staging on dist's filesystem for atomic per-file publication. The
# EXIT trap removes only this invocation's mktemp directory, including failures.
stage_root=""
cleanup() {
  if [[ -n "$stage_root" && -d "$stage_root" ]]; then
    rm -rf -- "$stage_root"
  fi
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
stage_root="$(mktemp -d "$dist/.agent-tunnel-package.XXXXXX")"

for variant in full connector; do
  features=()
  if [[ "$variant" == connector ]]; then features=(--no-default-features); fi
  cargo build --locked --release --target "$target" --target-dir "target/package-$variant" "${features[@]}"

  name="agent-tunnel-${version}-${target}-${variant}"
  bundle="$stage_root/$name"
  mkdir -p "$bundle"
  install -m 0755 "target/package-$variant/$target/release/agent-tunnel" "$bundle/agent-tunnel"
  install -m 0644 README.md SECURITY.md "$bundle/"
  if [[ "$variant" == full ]]; then
    mkdir -p "$bundle/skills/remote-debug" "$bundle/integrations/pi" "$bundle/docs"
    install -m 0644 skills/remote-debug/SKILL.md "$bundle/skills/remote-debug/SKILL.md"
    install -m 0644 integrations/pi/index.ts integrations/pi/ipc.mjs "$bundle/integrations/pi/"
    cp -a docs/. "$bundle/docs/"
  fi

  archive="agent-tunnel-${version}-${target}-${variant}.tar.gz"
  tar --sort=name --mtime="@$epoch" --owner=0 --group=0 --numeric-owner \
    -C "$stage_root" -cf - "$name" | gzip -n > "$stage_root/$archive"
  printf '%s: binary=%s bytes archive=%s bytes\n' "$name" \
    "$(wc -c < "$bundle/agent-tunnel")" "$(wc -c < "$stage_root/$archive")"
done

(
  cd "$stage_root"
  sha256sum -- "$full" "$connector" > "$manifest"
)
# Signing is opt-in; a checksum alone does NOT authenticate the publisher.
if [[ -n "${MINISIGN_SECRET_KEY:-}" ]]; then
  minisign -S -s "$MINISIGN_SECRET_KEY" -m "$stage_root/$manifest"
fi

# Recheck immediately before publishing. Without --force, hard-linking each
# staged file into dist is an atomic no-clobber operation. With --force, rename
# each staged file atomically over exactly its same-named destination.
check_collisions
publish() {
  local source="$1" output="$2"
  if ((force)); then
    mv -fT -- "$source" "$dist/$output"
  else
    ln -T -- "$source" "$dist/$output"
    rm -- "$source"
  fi
}
publish "$stage_root/$full" "$full"
publish "$stage_root/$connector" "$connector"
publish "$stage_root/$manifest" "$manifest"
if [[ -f "$stage_root/$signature" ]]; then
  publish "$stage_root/$signature" "$signature"
elif ((force)) && [[ -e "$dist/$signature" || -L "$dist/$signature" ]]; then
  # Do not leave a stale signature beside a newly generated unsigned manifest.
  rm -f -- "$dist/$signature"
fi
