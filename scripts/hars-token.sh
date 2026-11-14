#!/usr/bin/env bash
# Log in the way the frontend does and print an access token.
#
#   scripts/hars-token.sh URL USERNAME PASSWORD
#
# Uses only curl and sed. The client id is URL itself, as a browser on that
# origin would send it.
set -euo pipefail
url=${1%/}; user=$2; pass=$3
client="$url/"

field() { sed -n "s/.*\"$1\": *\"\([^\"]*\)\".*/\1/p"; }

flow=$(curl -sf -X POST "$url/auth/login_flow" \
  -d "{\"client_id\":\"$client\",\"handler\":[\"homeassistant\",null],\"redirect_uri\":\"$client\"}" | field flow_id)
[ -n "$flow" ] || { echo "login flow did not start" >&2; exit 1; }
code=$(curl -sf -X POST "$url/auth/login_flow/$flow" \
  -d "{\"client_id\":\"$client\",\"username\":\"$user\",\"password\":\"$pass\"}" | field result)
[ -n "$code" ] || { echo "login failed for $user" >&2; exit 1; }
token=$(curl -sf -X POST "$url/auth/token" \
  -F grant_type=authorization_code -F "client_id=$client" -F "code=$code" | field access_token)
[ -n "$token" ] || { echo "token exchange failed" >&2; exit 1; }
echo "$token"
