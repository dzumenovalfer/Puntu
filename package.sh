#!/usr/bin/env bash
# Make an installer archive for ANOTHER machine:  dist/puntu-<arch>-linux.tar.gz
#
# The same layout CI publishes on a GitHub release — the four binaries, the big Russian
# dictionary and install.sh — so the archive installs itself, offline:
#
#     tar xzf puntu-x86_64-linux.tar.gz && ./puntu/install.sh
#
#   ./package.sh             build with the `quick` profile (what ./install.sh uses) and pack
#   ./package.sh --release   the full `release` profile, as CI does — slower build, same code
#
# The binaries only run on systems whose C library is at least as new as the one they were
# built against; the script prints that floor at the end, and install.sh checks it on the
# target before replacing anything. For an OLDER target take the archive from the project's
# Releases page instead (CI builds it inside Ubuntu 22.04, for glibc 2.35 — see
# .github/workflows/release.yml), or build from source on the target.
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROFILE=quick
case "${1:-}" in
  "") ;;
  --release) PROFILE=release ;;
  -h|--help) sed -n '2,16p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
  *) echo "unknown option: $1 (see ./package.sh --help)" >&2; exit 1 ;;
esac

say() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }

say "Building ($PROFILE profile)…"
nice -n 10 cargo build --manifest-path "$REPO_DIR/Cargo.toml" --profile "$PROFILE" \
  --no-default-features --features ibus,app,gui --locked
TARGET="$REPO_DIR/target/$PROFILE"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/puntu"
cp "$TARGET/puntu" "$TARGET/puntu-ibus" "$TARGET/puntu-app" "$TARGET/puntu-gui" \
   "$REPO_DIR/install.sh" "$STAGE/puntu/"

say "Building the big Russian dictionary (≈1.5M words → ~2MB FST)…"
"$TARGET/puntu" build-dict "$REPO_DIR/dictionaries/russian.utf-8" "$STAGE/puntu/russian.fst" \
  2>/dev/null

ARCH="$(uname -m)"
OUT="$REPO_DIR/dist/puntu-$ARCH-linux.tar.gz"
mkdir -p "$REPO_DIR/dist"
# Fixed owner and order, so two runs over the same binaries give the same archive.
tar -C "$STAGE" --owner=0 --group=0 --numeric-owner --sort=name -czf "$OUT" puntu

say "Wrote $OUT ($(du -h "$OUT" | cut -f1), $("$TARGET/puntu" --version))"
if command -v objdump >/dev/null 2>&1; then
  echo "    Needs at least this glibc on the target (this machine: $(ldd --version | head -n1 | grep -o '[0-9.]*$')):"
  for bin in puntu puntu-ibus puntu-gui puntu-app; do
    need="$(objdump -T "$TARGET/$bin" 2>/dev/null | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -n1)"
    printf '      %-11s %s\n' "$bin" "${need#GLIBC_}"
  done
  echo "    For an older system: the archive from the Releases page is built for glibc 2.35."
fi
cat <<EOF

On the other machine (Ubuntu/Debian with GNOME; no Rust and no network needed):

    tar xzf $(basename "$OUT") && ./puntu/install.sh

EOF
