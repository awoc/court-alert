#!/usr/bin/env bash

set -euo pipefail

DEPLOY_HOST="${DEPLOY_HOST:-court-alert.local}"
DEPLOY_USER="${DEPLOY_USER:-pi}"
DEPLOY_TARGET="${DEPLOY_TARGET:-armv7-unknown-linux-gnueabihf}"
REMOTE_DIR="${REMOTE_DIR:-/opt/court-alert}"
SERVICE="${SERVICE:-court-alert}"
REMOTE="${DEPLOY_USER}@${DEPLOY_HOST}"

cd "$(dirname "$0")"

[[ -f config.toml ]] || { echo "config.toml not found in $(pwd)" >&2; exit 1; }

echo "==> Building court-alert (release) for ${DEPLOY_TARGET}"
if command -v cross >/dev/null 2>&1; then
  cross build --release --target "${DEPLOY_TARGET}"
else
  echo "    cross not installed — falling back to 'cargo build' (requires the target toolchain locally)"
  echo "    install with: cargo install cross --git https://github.com/cross-rs/cross"
  cargo build --release --target "${DEPLOY_TARGET}"
fi

BINARY="target/${DEPLOY_TARGET}/release/court-alert"
[[ -f "${BINARY}" ]] || { echo "build artifact not found at ${BINARY}" >&2; exit 1; }
echo "    artifact: ${BINARY} ($(du -h "${BINARY}" | cut -f1))"

echo "==> Copying to ${REMOTE}:/tmp/"
FILES=("${BINARY}" config.toml)
scp -O -q "${FILES[@]}" "${REMOTE}:/tmp/"

echo "==> Installing on remote and restarting ${SERVICE}"
ssh "${REMOTE}" "bash -s" <<EOF
set -euo pipefail
sudo install -o ${SERVICE} -g ${SERVICE} -m 755 /tmp/court-alert ${REMOTE_DIR}/court-alert
sudo install -o ${SERVICE} -g ${SERVICE} -m 644 /tmp/config.toml ${REMOTE_DIR}/config.toml
rm -f /tmp/court-alert /tmp/config.toml
sudo systemctl enable ${SERVICE} >/dev/null
sudo systemctl restart ${SERVICE}
sleep 2
sudo systemctl --no-pager --lines=10 status ${SERVICE}
EOF

echo
echo "==> Deploy complete."
echo "    Tail logs:    ssh ${REMOTE} 'journalctl -u ${SERVICE} -f'"
echo "    Service ctl:  ssh ${REMOTE} 'sudo systemctl <start|stop|restart|status> ${SERVICE}'"
