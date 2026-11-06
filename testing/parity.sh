#!/usr/bin/env bash
# Java↔Rust endpoint parity check.
#
# Requires both backends running:
#   task backend:dev   (Java on :8080)
#   task rust:dev      (Rust on :8081)
#
# Usage: testing/parity.sh [name-filter]
#   JAVA_URL / RUST_URL override the base URLs.
#
# Kinds: json (jq -S compare), pdf (pdfdiff structural compare),
#        zip (entry list + per-entry pdfdiff).

set -u

JAVA="${JAVA_URL:-http://localhost:8080}"
RUST="${RUST_URL:-http://localhost:8081}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

PDF4="$ROOT/app/core/src/test/resources/pdftexteditor/mushroom-life.pdf"   # 4 pages
PDF1="$ROOT/testing/test_pdf_1.pdf"
PDF2="$ROOT/testing/test_pdf_2.pdf"
CROP="$ROOT/testing/crop_test.pdf"

PDFDIFF_BIN="$ROOT/rust/target/debug/examples/pdfdiff"
if [ ! -x "$PDFDIFF_BIN" ]; then
    cargo build --quiet --example pdfdiff --manifest-path "$ROOT/rust/Cargo.toml" || exit 1
fi

PASS=0
FAIL=0
FAILED_CASES=()

ok()  { PASS=$((PASS + 1)); printf '  PASS %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); FAILED_CASES+=("$1"); printf '  FAIL %s\n' "$1"; }

fetch() { # base path out -F args...
    local base="$1" path="$2" out="$3"; shift 3
    curl -sS -o "$out" -w "%{http_code}" -X POST "$base$path" "$@"
}

check_json() { # name path curl-args...
    local name="$1" path="$2"; shift 2
    local ja jb ca cb
    ja=$(mktemp); jb=$(mktemp)
    ca=$(fetch "$JAVA" "$path" "$ja" "$@")
    cb=$(fetch "$RUST" "$path" "$jb" "$@")
    if [ "$ca" != "$cb" ]; then
        bad "$name (status java=$ca rust=$cb)"
    elif [ "$ca" != "200" ]; then
        # Matching rejection is parity too (error-shape differences are
        # tracked in MIGRATION.md, not diffed here).
        ok "$name (both $ca)"
    elif ! diff <(jq -S . "$ja" 2>/dev/null) <(jq -S . "$jb" 2>/dev/null) >/dev/null; then
        bad "$name (json diff)"
        diff <(jq -S . "$ja") <(jq -S . "$jb") | head -20 | sed 's/^/    /'
    else
        ok "$name"
    fi
    rm -f "$ja" "$jb"
}

check_pdf() { # name path curl-args...
    local name="$1" path="$2"; shift 2
    local ja jb ca cb
    ja=$(mktemp); jb=$(mktemp)
    ca=$(fetch "$JAVA" "$path" "$ja" "$@")
    cb=$(fetch "$RUST" "$path" "$jb" "$@")
    if [ "$ca" != "$cb" ]; then
        bad "$name (status java=$ca rust=$cb)"
    elif [ "$ca" != "200" ]; then
        ok "$name (both $ca)"
    elif ! "$PDFDIFF_BIN" "$ja" "$jb" >/tmp/pdfdiff.$$.$name.out 2>&1; then
        bad "$name (pdfdiff)"
        head -20 "/tmp/pdfdiff.$$.$name.out" | sed 's/^/    /'
    else
        ok "$name"
    fi
    rm -f "$ja" "$jb" "/tmp/pdfdiff.$$.$name.out"
}

check_zip() { # name path curl-args...
    local name="$1" path="$2"; shift 2
    local ja jb ca cb da db
    ja=$(mktemp); jb=$(mktemp)
    ca=$(fetch "$JAVA" "$path" "$ja" "$@")
    cb=$(fetch "$RUST" "$path" "$jb" "$@")
    da=$(mktemp -d); db=$(mktemp -d)
    if [ "$ca" != "$cb" ]; then
        bad "$name (status java=$ca rust=$cb)"
    elif [ "$ca" != "200" ]; then
        ok "$name (both $ca)"
    else
        (cd "$da" && unzip -oq "$ja")
        (cd "$db" && unzip -oq "$jb")
        local la lb
        la=$(cd "$da" && ls | sort); lb=$(cd "$db" && ls | sort)
        if [ "$la" != "$lb" ]; then
            bad "$name (zip entries differ)"
            diff <(echo "$la") <(echo "$lb") | head -10 | sed 's/^/    /'
        else
            local entry rc=0
            for entry in $la; do
                case "$entry" in
                    *.pdf) "$PDFDIFF_BIN" "$da/$entry" "$db/$entry" >/dev/null 2>&1 || rc=1 ;;
                esac
            done
            if [ "$rc" = 0 ]; then ok "$name"; else bad "$name (zip pdfdiff)"; fi
        fi
    fi
    rm -rf "$ja" "$jb" "$da" "$db"
}

run() { # kind name path args...
    local kind="$1" name="$2" path="$3"; shift 3
    if [ -n "${FILTER:-}" ] && [[ "$name" != *"$FILTER"* ]]; then return; fi
    case "$kind" in
        json) check_json "$name" "$path" "$@" ;;
        pdf)  check_pdf  "$name" "$path" "$@" ;;
        zip)  check_zip  "$name" "$path" "$@" ;;
    esac
}

FILTER="${1:-}"

echo "== analysis (json) =="
for ep in page-count basic-info page-dimensions document-properties form-fields annotation-info font-info security-info; do
    run json "analysis/$ep" "/api/v1/analysis/$ep" -F "fileInput=@$PDF4"
done

echo "== general =="
run pdf "rotate-pdf" "/api/v1/general/rotate-pdf" -F "fileInput=@$PDF4" -F "angle=90"
run pdf "remove-pages" "/api/v1/general/remove-pages" -F "fileInput=@$PDF4" -F "pageNumbers=2"
run pdf "rearrange-pages" "/api/v1/general/rearrange-pages" -F "fileInput=@$PDF4" -F "customMode=REVERSE_ORDER"
run pdf "merge-pdfs" "/api/v1/general/merge-pdfs" -F "fileInput=@$PDF4" -F "fileInput=@$PDF1"
run zip "split-pages" "/api/v1/general/split-pages" -F "fileInput=@$PDF4" -F "pageNumbers=1-2,4"
run pdf "remove-image-pdf" "/api/v1/general/remove-image-pdf" -F "fileInput=@$PDF4"
run pdf "scale-pages" "/api/v1/general/scale-pages" -F "fileInput=@$PDF4" -F "scaleFactor=0.5"
run pdf "crop" "/api/v1/general/crop" -F "fileInput=@$CROP" -F "x=10" -F "y=10" -F "width=200" -F "height=200" -F "removeDataOutsideCrop=false"
run zip "split-by-size-or-count" "/api/v1/general/split-by-size-or-count" -F "fileInput=@$PDF4" -F "splitType=0" -F "splitValue=2"
run zip "split-pdf-by-chapters" "/api/v1/general/split-pdf-by-chapters" -F "fileInput=@$PDF4"

echo "== misc =="
run pdf "decompress-pdf" "/api/v1/misc/decompress-pdf" -F "fileInput=@$PDF4"
run pdf "update-metadata" "/api/v1/misc/update-metadata" -F "fileInput=@$PDF4" -F "title=Parity"
run pdf "flatten" "/api/v1/misc/flatten" -F "fileInput=@$PDF4"
run pdf "compress-pdf" "/api/v1/misc/compress-pdf" -F "fileInput=@$PDF4"
run json "show-javascript" "/api/v1/misc/show-javascript" -F "fileInput=@$PDF4"

echo "== security =="
run pdf "sanitize-pdf" "/api/v1/security/sanitize-pdf" -F "fileInput=@$PDF4"
run pdf "remove-cert-sign" "/api/v1/security/remove-cert-sign" -F "fileInput=@$PDF4"

echo
echo "== $PASS passed, $FAIL failed =="
if [ "$FAIL" -gt 0 ]; then
    printf 'failed: %s\n' "${FAILED_CASES[@]}"
    exit 1
fi
