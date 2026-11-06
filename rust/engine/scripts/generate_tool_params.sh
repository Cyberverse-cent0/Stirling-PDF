#!/usr/bin/env bash
# Regenerate src/models/tool_params.rs from the Java OpenAPI spec.
#
# Prerequisites: a running Java backend serving /v3/api-docs (default :8080),
# or pass a spec path directly: ./scripts/generate_tool_params.sh /path/to/SwaggerDoc.json
set -euo pipefail
cd "$(dirname "$0")/.."

SPEC="${1:-}"
if [[ -z "$SPEC" ]]; then
    JAVA_URL="${JAVA_URL:-http://localhost:8080}"
    SPEC="$(mktemp -t SwaggerDoc.XXXXXX.json)"
    trap 'rm -f "$SPEC"' EXIT
    curl -sf "$JAVA_URL/v3/api-docs" -o "$SPEC"
fi

cargo run --example generate_tool_params -- "$SPEC"
