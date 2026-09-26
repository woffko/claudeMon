#!/usr/bin/env bash
set -euo pipefail

print_usage() {
  cat <<'EOF'
Usage: install-prebuilt.sh [root]

Args:
  root    Install root (default: ~/.local)

Installs the bundled `claudemon` binary from this package into <root>/bin.
EOF
}

ROOT="${1:-$HOME/.local}"

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
  print_usage
  exit 0
fi

if [ $# -gt 1 ]; then
  echo "Too many arguments" >&2
  print_usage >&2
  exit 1
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
SRC_BIN="${SCRIPT_DIR}/claudemon"
CLAUDEMON_HOME_DIR="${CLAUDEMON_HOME:-$HOME/.claudemon}"

if [ ! -f "${SRC_BIN}" ]; then
  echo "Missing binary: ${SRC_BIN}" >&2
  exit 1
fi

if [ -L "${CLAUDEMON_HOME_DIR}" ]; then
  echo "Refusing to use CLAUDEMON_HOME (${CLAUDEMON_HOME_DIR}): symlink is not allowed." >&2
  exit 1
fi

if [ -e "${CLAUDEMON_HOME_DIR}" ] && [ ! -d "${CLAUDEMON_HOME_DIR}" ]; then
  echo "Refusing to use CLAUDEMON_HOME (${CLAUDEMON_HOME_DIR}): expected a directory." >&2
  exit 1
fi

mkdir -p "${CLAUDEMON_HOME_DIR}"
chmod 700 "${CLAUDEMON_HOME_DIR}" 2>/dev/null || true

BIN_DIR="${ROOT}/bin"
DST_BIN="${BIN_DIR}/claudemon"

mkdir -p "${BIN_DIR}"

if [ -L "${DST_BIN}" ]; then
  echo "Refusing to overwrite symlink: ${DST_BIN}" >&2
  exit 1
fi

install -m 755 "${SRC_BIN}" "${DST_BIN}"

echo "Installed claudemon to ${DST_BIN}"
echo "Prepared CLAUDEMON_HOME at ${CLAUDEMON_HOME_DIR}"
case ":${PATH}:" in
  *":${BIN_DIR}:"*) ;;
  *) echo "Add to PATH: export PATH=\"${BIN_DIR}:\$PATH\"" ;;
esac
