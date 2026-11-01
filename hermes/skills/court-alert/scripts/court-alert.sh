#!/usr/bin/env bash
# Signed string must match docs/adr/0001-hmac-signed-webhook-api.md.
set -euo pipefail

if [[ $# -lt 2 || $# -gt 3 ]]; then
  echo "usage: $0 METHOD PATH [JSON_BODY]" >&2
  exit 2
fi
: "${COURT_ALERT_URL:?set COURT_ALERT_URL to the court-alert API base URL}"
: "${COURT_ALERT_API_SECRET:?set COURT_ALERT_API_SECRET to the API secret}"

method=$(printf '%s' "$1" | tr '[:lower:]' '[:upper:]')
path=$2
body=${3:-}
timestamp=$(date +%s)

signature=$(printf '%s.%s.%s.%s' "$timestamp" "$method" "$path" "$body" |
  openssl dgst -sha256 -hmac "$COURT_ALERT_API_SECRET" -r | cut -d' ' -f1)

response_file=$(mktemp)
trap 'rm -f "$response_file"' EXIT

curl_args=(
  --silent --show-error
  --request "$method"
  --header "X-Webhook-Timestamp: $timestamp"
  --header "X-Webhook-Signature-V2: $signature"
  --output "$response_file"
  --write-out '%{http_code}'
)
if [[ -n $body ]]; then
  curl_args+=(--header 'Content-Type: application/json' --data-binary "$body")
fi

status=$(curl "${curl_args[@]}" "${COURT_ALERT_URL%/}$path")
cat "$response_file"
echo "HTTP $status" >&2
[[ $status == 2* ]]
