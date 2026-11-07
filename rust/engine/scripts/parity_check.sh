#!/usr/bin/env bash
# Parity harness: compare Python and Rust engine responses on the
# deterministic endpoints (no LLM calls). AI endpoints need live provider
# credentials and are compared manually during cutover.
#
# Prerequisites:
#   - Python engine on $PY_URL (default http://localhost:5001)
#   - Rust engine on $RS_URL (default http://localhost:5002)
#
#   PY_URL=http://localhost:5001 RS_URL=http://localhost:5002 ./scripts/parity_check.sh
set -euo pipefail
cd "$(dirname "$0")/.."

PY_URL="${PY_URL:-http://localhost:5001}"
RS_URL="${RS_URL:-http://localhost:5002}"
USER_ID="${USER_ID:-parity-test}"
FAILURES=0

check() {
    local name="$1" method="$2" path="$3" body="${4:-}"
    local py_out rs_out
    if [[ -n "$body" ]]; then
        py_out=$(curl -sf -X "$method" "$PY_URL$path" -H "X-User-Id: $USER_ID" \
            -H 'Content-Type: application/json' -d "$body")
        rs_out=$(curl -sf -X "$method" "$RS_URL$path" -H "X-User-Id: $USER_ID" \
            -H 'Content-Type: application/json' -d "$body")
    else
        py_out=$(curl -sf -X "$method" "$PY_URL$path" -H "X-User-Id: $USER_ID")
        rs_out=$(curl -sf -X "$method" "$RS_URL$path" -H "X-User-Id: $USER_ID")
    fi
    if [[ "$(echo "$py_out" | jq -S .)" == "$(echo "$rs_out" | jq -S .)" ]]; then
        echo "PASS  $name"
    else
        echo "FAIL  $name"
        diff <(echo "$py_out" | jq -S .) <(echo "$rs_out" | jq -S .) | head -40 || true
        FAILURES=$((FAILURES + 1))
    fi
}

echo "== health =="
check "health" GET "/health"

echo "== docparse (deterministic: index=false, includeChunks) =="
DOCparse_BODY='{
  "documentId": "parity-doc-1",
  "source": "parity",
  "index": false,
  "includeChunks": true,
  "chunkSize": 200,
  "overlap": 40,
  "blocks": [
    {"markdown": "# Title\n\nFirst paragraph with some words.", "pageStart": 1, "pageEnd": 1, "headingPath": ["Title"]},
    {"markdown": "Second paragraph.\n\n- list item one\n- list item two", "pageStart": 2, "pageEnd": 2},
    {"markdown": "A much longer paragraph that should force the packer to split across the configured chunk size boundary with overlap carried between chunks.", "pageStart": 3, "pageEnd": 3, "headingPath": ["Title", "Details"]}
  ]
}'
check "docparse ingest chunks" POST "/api/v1/docparse/ingest" "$DOCparse_BODY"

echo "== docparse validation errors =="
# 422 bodies legitimately differ (pydantic per-field list vs single detail
# string), so compare status codes only.
check_status() {
    local name="$1" method="$2" path="$3" body="${4:-}"
    local py_code rs_code
    py_code=$(curl -s -o /dev/null -w '%{http_code}' -X "$method" "$PY_URL$path" \
        -H "X-User-Id: $USER_ID" -H 'Content-Type: application/json' -d "$body")
    rs_code=$(curl -s -o /dev/null -w '%{http_code}' -X "$method" "$RS_URL$path" \
        -H "X-User-Id: $USER_ID" -H 'Content-Type: application/json' -d "$body")
    if [[ "$py_code" == "$rs_code" ]]; then
        echo "PASS  $name ($py_code)"
    else
        echo "FAIL  $name py=$py_code rs=$rs_code"
        FAILURES=$((FAILURES + 1))
    fi
}
check_status "docparse empty blocks 422" POST "/api/v1/docparse/ingest" \
    '{"documentId":"x","source":"s","index":false,"includeChunks":true,"blocks":[]}'
check_status "docparse overlap>=size 422" POST "/api/v1/docparse/ingest" \
    '{"documentId":"x","source":"s","index":false,"includeChunks":true,"chunkSize":100,"overlap":100,"blocks":[{"markdown":"hi"}]}'
check_status "docparse nothing-to-do 422" POST "/api/v1/docparse/ingest" \
    '{"documentId":"x","source":"s","index":false,"includeChunks":false,"blocks":[{"markdown":"hi"}]}'

echo "== wire-format parity (snake_case aliases, ISO datetimes, 422s) =="
# Bodies are identical on both engines for these — full-body compare.
check "docparse snake_case body" POST "/api/v1/docparse/ingest" \
    '{"document_id":"snake-1","source":"s","index":false,"include_chunks":true,"blocks":[{"markdown":"hi","page_start":1,"page_end":1}]}'
check "docparse expires_at RFC3339" POST "/api/v1/docparse/ingest" \
    '{"documentId":"dt-1","source":"s","index":false,"includeChunks":true,"expiresAt":"2030-01-02T03:04:05Z","blocks":[{"markdown":"hi","pageStart":1,"pageEnd":1}]}'
check_status "docparse expires_at date-only" POST "/api/v1/docparse/ingest" \
    '{"documentId":"dt-2","source":"s","index":false,"includeChunks":true,"expiresAt":"2030-01-02","blocks":[{"markdown":"hi","pageStart":1,"pageEnd":1}]}'
check_status "docparse expires_at space-sep naive" POST "/api/v1/docparse/ingest" \
    '{"documentId":"dt-3","source":"s","index":false,"includeChunks":true,"expiresAt":"2030-01-02 03:04:05","blocks":[{"markdown":"hi","pageStart":1,"pageEnd":1}]}'
check_status "docparse malformed json 422" POST "/api/v1/docparse/ingest" '{bad'
check_status "docparse unknown field 422" POST "/api/v1/docparse/ingest" \
    '{"documentId":"x","bogus":1,"blocks":[{"markdown":"hi","pageStart":1,"pageEnd":1}]}'
check_status "docparse chunkSize range 422" POST "/api/v1/docparse/ingest" \
    '{"documentId":"x","source":"s","index":false,"includeChunks":true,"chunkSize":10,"blocks":[{"markdown":"hi","pageStart":1,"pageEnd":1}]}'
check_status "docparse empty documentId 422" POST "/api/v1/docparse/ingest" \
    '{"documentId":"","source":"s","index":false,"includeChunks":true,"blocks":[{"markdown":"hi","pageStart":1,"pageEnd":1}]}'

echo "== auth =="
# Both engines must reject a protected route without X-User-Id when
# STIRLING_REQUIRE_USER_ID=true; with it unset the gate is off, so only
# check that the header is accepted identically (401 vs 401).
PY_NOU=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$PY_URL/api/v1/documents" \
    -H 'Content-Type: application/json' -d '{}')
RS_NOU=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$RS_URL/api/v1/documents" \
    -H 'Content-Type: application/json' -d '{}')
if [[ "$PY_NOU" == "$RS_NOU" ]]; then
    echo "PASS  documents no-user status ($PY_NOU)"
else
    echo "FAIL  documents no-user status py=$PY_NOU rs=$RS_NOU"
    FAILURES=$((FAILURES + 1))
fi

if [[ $FAILURES -gt 0 ]]; then
    echo "== $FAILURES parity failure(s) =="
    exit 1
fi
echo "== all parity checks passed =="
