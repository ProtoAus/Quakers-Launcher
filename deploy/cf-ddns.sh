#!/usr/bin/env bash
# =============================================================================
# Keep a DNS-only Cloudflare A record pointed at this box's current public IP.
#
# WHY: play.proto.bar has to resolve to the real home IP, because the game server is
# UDP (FTE listens on 27500) and Cloudflare's proxy carries TCP only on self-serve
# plans -- a proxied record answers with an anycast address that silently drops every
# game packet. So the record must stay grey-clouded, which in turn means it breaks the
# moment the ISP rotates the address. This puts it back.
#
# Config lives in /etc/cf-ddns.conf (root-owned, 0600):
#   CF_API_TOKEN=...        # Zone -> DNS -> Edit on the zone below
#   CF_ZONE=proto.bar
#   CF_RECORD=play.proto.bar
#
# Install: see cf-ddns.service / cf-ddns.timer beside this file.
# =============================================================================
set -uo pipefail

CONF=${CONF:-/etc/cf-ddns.conf}
API=https://api.cloudflare.com/client/v4

log() { echo "cf-ddns: $*"; }
die() { log "$*"; exit 1; }

[ -r "$CONF" ] || { log "no $CONF yet - not configured, doing nothing"; exit 0; }
# shellcheck source=/dev/null
. "$CONF"

: "${CF_API_TOKEN:=}" ; : "${CF_ZONE:=}" ; : "${CF_RECORD:=}"
case "$CF_API_TOKEN" in
    ""|PUT-TOKEN-HERE) log "token not set in $CONF - doing nothing"; exit 0 ;;
esac
[ -n "$CF_ZONE" ] && [ -n "$CF_RECORD" ] || die "CF_ZONE and CF_RECORD must be set in $CONF"

api() {  # api <method> <path> [json-body]
    local method=$1 path=$2 body=${3:-}
    if [ -n "$body" ]; then
        curl -4 -sS --max-time 20 -X "$method" "$API$path" \
             -H "Authorization: Bearer $CF_API_TOKEN" \
             -H "Content-Type: application/json" --data "$body"
    else
        curl -4 -sS --max-time 20 -X "$method" "$API$path" \
             -H "Authorization: Bearer $CF_API_TOKEN"
    fi
}

jget() { python3 -c "import sys,json;d=json.load(sys.stdin);print(eval('d'+sys.argv[1]) if d else '')" "$1" 2>/dev/null; }

# --- current public IP -------------------------------------------------------
# Cloudflare's own trace endpoint: no third-party dependency, and it is the same
# network that has to agree with us about what our address is. -4 is required --
# this connection has no working IPv6 path and an AAAA answer would hang.
MYIP=$(curl -4 -sS --max-time 15 https://cloudflare.com/cdn-cgi/trace | sed -n 's/^ip=//p')
case "$MYIP" in
    *.*.*.*) : ;;
    *) die "could not determine public IPv4 (got '${MYIP:-empty}')" ;;
esac

# --- resolve zone + record ---------------------------------------------------
ZONE_ID=$(api GET "/zones?name=$CF_ZONE" | jget "['result'][0]['id']")
[ -n "$ZONE_ID" ] || die "zone $CF_ZONE not found, or the token lacks Zone:Read"

REC=$(api GET "/zones/$ZONE_ID/dns_records?type=A&name=$CF_RECORD")
REC_ID=$(echo "$REC" | jget "['result'][0]['id']")
REC_IP=$(echo "$REC" | jget "['result'][0]['content']")
REC_PROXIED=$(echo "$REC" | jget "['result'][0]['proxied']")

# `"proxied": false` is sent on EVERY write, never omitted. Orange-clouding this record
# would take the game server offline in a way that looks like the server is down rather
# than misrouted, so it is restated explicitly rather than left to the API's defaults.
BODY=$(printf '{"type":"A","name":"%s","content":"%s","ttl":60,"proxied":false}' "$CF_RECORD" "$MYIP")

if [ -z "$REC_ID" ]; then
    RESP=$(api POST "/zones/$ZONE_ID/dns_records" "$BODY")
    [ "$(echo "$RESP" | jget "['success']")" = "True" ] \
        || die "create failed: $(echo "$RESP" | jget "['errors']")"
    log "created $CF_RECORD -> $MYIP (DNS only)"
    exit 0
fi

if [ "$REC_IP" = "$MYIP" ] && [ "$REC_PROXIED" = "False" ]; then
    exit 0   # nothing to do; stay quiet so the journal only carries real events
fi

RESP=$(api PATCH "/zones/$ZONE_ID/dns_records/$REC_ID" "$BODY")
[ "$(echo "$RESP" | jget "['success']")" = "True" ] \
    || die "update failed: $(echo "$RESP" | jget "['errors']")"

if [ "$REC_IP" != "$MYIP" ]; then
    log "$CF_RECORD: $REC_IP -> $MYIP"
else
    log "$CF_RECORD: re-set to DNS-only (was proxied, which breaks UDP)"
fi
