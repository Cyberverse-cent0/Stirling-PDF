#!/usr/bin/env python3
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import re
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
import uuid
import zipfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

FILE_FIELD = "fileInput"
DEFAULT_JAVA = "http://localhost:8080"
DEFAULT_RUST = "http://localhost:8081"
DEFAULT_FINGERPRINT = "rust/target/debug/examples/pdf_fingerprint"


@dataclass(frozen=True)
class Case:
    name: str
    path: str
    expect: str
    fields: tuple[tuple[str, str], ...] = ()
    files: str = "single"
    ignore_info: tuple[str, ...] = ()


CASES = [
    Case("analysis/page-count", "/api/v1/analysis/page-count", "json"),
    Case("analysis/basic-info", "/api/v1/analysis/basic-info", "json"),
    Case("analysis/page-dimensions", "/api/v1/analysis/page-dimensions", "json"),
    Case("analysis/document-properties", "/api/v1/analysis/document-properties", "json"),
    Case("analysis/form-fields", "/api/v1/analysis/form-fields", "json"),
    Case("analysis/annotation-info", "/api/v1/analysis/annotation-info", "json"),
    Case("analysis/font-info", "/api/v1/analysis/font-info", "json"),
    Case("analysis/security-info", "/api/v1/analysis/security-info", "json"),
    Case("general/rotate-pdf", "/api/v1/general/rotate-pdf", "pdf", (("angle", "90"),)),
    Case("general/remove-pages", "/api/v1/general/remove-pages", "pdf", (("pageNumbers", "1"),)),
    Case("general/rearrange-pages", "/api/v1/general/rearrange-pages", "pdf", (("pageNumbers", "all"),)),
    Case(
        "general/rearrange-pages-reverse",
        "/api/v1/general/rearrange-pages",
        "pdf",
        (("customMode", "REVERSE_ORDER"),),
    ),
    Case("general/split-pages", "/api/v1/general/split-pages", "zip", (("pageNumbers", "1"),)),
    Case(
        "general/split-by-size-or-count",
        "/api/v1/general/split-by-size-or-count",
        "zip",
        (("splitType", "1"), ("splitValue", "2")),
    ),
    Case(
        "general/split-pdf-by-chapters",
        "/api/v1/general/split-pdf-by-chapters",
        "zip",
        (("bookmarkLevel", "1"), ("includeMetadata", "false")),
    ),
    Case(
        "general/split-pdf-by-sections",
        "/api/v1/general/split-pdf-by-sections",
        "zip",
        (
            ("splitMode", "SPLIT_ALL"),
            ("horizontalDivisions", "1"),
            ("verticalDivisions", "1"),
            ("merge", "false"),
        ),
    ),
    Case(
        "general/split-pdf-by-sections-merge",
        "/api/v1/general/split-pdf-by-sections",
        "pdf",
        (
            ("splitMode", "SPLIT_ALL"),
            ("horizontalDivisions", "1"),
            ("verticalDivisions", "0"),
            ("merge", "true"),
        ),
    ),
    Case(
        "general/scale-pages-a4",
        "/api/v1/general/scale-pages",
        "pdf",
        (("pageSize", "A4"), ("orientation", "PORTRAIT"), ("scaleFactor", "1")),
    ),
    Case("general/scale-pages-keep", "/api/v1/general/scale-pages", "pdf", (("pageSize", "KEEP"),)),
    Case(
        "general/crop",
        "/api/v1/general/crop",
        "pdf",
        (
            ("x", "50"),
            ("y", "50"),
            ("width", "200"),
            ("height", "200"),
            ("pageNumbers", "all"),
            ("autoCrop", "false"),
            ("removeDataOutsideCrop", "false"),
        ),
    ),
    Case("general/pdf-to-single-page", "/api/v1/general/pdf-to-single-page", "pdf"),
    Case("general/merge-pdfs", "/api/v1/general/merge-pdfs", "pdf", files="pair"),
    Case("misc/decompress-pdf", "/api/v1/misc/decompress-pdf", "pdf"),
    Case(
        "misc/update-metadata",
        "/api/v1/misc/update-metadata",
        "pdf",
        (
            ("title", "Parity Title"),
            ("author", "Parity Author"),
            ("subject", "Parity Subject"),
            ("keywords", "parity, rust, java"),
            ("creator", "Parity Creator"),
            ("producer", "Parity Producer"),
            ("creationDate", "2024/01/02 03:04:05"),
            ("modificationDate", "2024/02/03 04:05:06"),
            ("trapped", "False"),
        ),
    ),
    Case("misc/flatten-forms", "/api/v1/misc/flatten", "pdf", (("flattenOnlyForms", "true"),)),
    Case(
        "misc/compress-pdf",
        "/api/v1/misc/compress-pdf",
        "pdf",
        (
            ("optimizeLevel", "1"),
            ("linearize", "false"),
            ("normalize", "false"),
            ("grayscale", "false"),
        ),
    ),
    Case("security/remove-cert-sign", "/api/v1/security/remove-cert-sign", "pdf"),
]

KNOWN_DIFFS = [
    ("document-properties", re.compile(r"^(creationDate|modificationDate)$")),
    ("security-info", re.compile(r"^keyLength$")),
    ("font-info", re.compile(r"^(fonts|fontCount)$")),
    ("form-fields", re.compile(r"^fieldCount$")),
    ("scale-pages", re.compile(r"^pages\[\d+\]\.content(Operations|Operands|Operators)$")),
    ("crop", re.compile(r"^pages\[\d+\]\.content(Operations|Operands|Operators)$")),
]


@dataclass
class Response:
    status: int
    headers: dict[str, str]
    body: bytes
    error: str | None = None


@dataclass
class Diff:
    path: str
    java: Any
    rust: Any
    kind: str = "semantic"


@dataclass
class Result:
    case: str
    input_name: str
    java_status: int
    rust_status: int
    semantic: list[Diff] = field(default_factory=list)
    known: list[Diff] = field(default_factory=list)
    structure: list[Diff] = field(default_factory=list)
    note: str = ""

    @property
    def failed(self) -> bool:
        return bool(self.semantic)


def multipart_body(files: list[tuple[str, str, bytes]], fields: list[tuple[str, str]]) -> tuple[bytes, str]:
    boundary = f"----stirling-parity-{uuid.uuid4().hex}"
    chunks: list[bytes] = []
    for name, value in fields:
        chunks.extend(
            [
                f"--{boundary}\r\n".encode(),
                f'Content-Disposition: form-data; name="{name}"\r\n\r\n'.encode(),
                value.encode(),
                b"\r\n",
            ]
        )
    for field_name, filename, data in files:
        chunks.extend(
            [
                f"--{boundary}\r\n".encode(),
                (
                    f'Content-Disposition: form-data; name="{field_name}"; '
                    f'filename="{filename}"\r\n'
                ).encode(),
                b"Content-Type: application/pdf\r\n\r\n",
                data,
                b"\r\n",
            ]
        )
    chunks.append(f"--{boundary}--\r\n".encode())
    return b"".join(chunks), f"multipart/form-data; boundary={boundary}"


def post_multipart(base: str, path: str, files: list[tuple[str, str, bytes]], fields: list[tuple[str, str]], timeout: float, extra_headers: dict[str, str] | None = None) -> Response:
    body, content_type = multipart_body(files, fields)
    headers = {"Content-Type": content_type, "Accept": "*/*"}
    if extra_headers:
        headers.update(extra_headers)
    request = urllib.request.Request(
        base.rstrip("/") + path,
        data=body,
        headers=headers,
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return Response(response.status, {k.lower(): v for k, v in response.headers.items()}, response.read())
    except urllib.error.HTTPError as error:
        return Response(error.code, {k.lower(): v for k, v in error.headers.items()}, error.read())
    except Exception as error:
        return Response(0, {}, b"", str(error))


def discover_inputs(root: Path, include_testing: bool) -> list[Path]:
    patterns = ["app/**/src/test/resources/**/*.pdf", "app/**/test/resources/**/*.pdf"]
    if include_testing:
        patterns += [
            "testing/test_pdf_*.pdf",
            "testing/crop_test.pdf",
            "testing/ledger/*.pdf",
            "testing/cucumber/exampleFiles/*.pdf",
        ]
    inputs = {path for pattern in patterns for path in root.glob(pattern) if path.is_file()}
    return sorted(inputs)


def ensure_fingerprint(root: Path, args: argparse.Namespace) -> Path:
    configured = Path(args.fingerprint)
    if configured.is_absolute():
        return configured
    configured = root / configured
    if configured.exists() and not args.build_fingerprint:
        return configured
    subprocess.run(
        ["cargo", "build", "--manifest-path", "rust/Cargo.toml", "--example", "pdf_fingerprint"],
        cwd=root,
        check=True,
    )
    return configured


class FingerprintCache:
    def __init__(self, binary: Path):
        self.binary = binary
        self.cache: dict[str, Any] = {}
        self.tempdir = tempfile.TemporaryDirectory(prefix="stirling-pdf-parity-")

    def get(self, data: bytes) -> Any:
        key = hashlib.sha256(data).hexdigest()
        if key not in self.cache:
            path = Path(self.tempdir.name) / f"{key}.pdf"
            path.write_bytes(data)
            completed = subprocess.run([str(self.binary), str(path)], capture_output=True, text=True, timeout=120)
            if completed.returncode != 0 and not completed.stdout.strip():
                self.cache[key] = {"ok": False, "error": completed.stderr.strip() or "fingerprint failed"}
            else:
                try:
                    self.cache[key] = json.loads(completed.stdout)
                except json.JSONDecodeError:
                    self.cache[key] = {"ok": False, "error": completed.stdout.strip() or completed.stderr.strip()}
        return self.cache[key]


def semantic_view(fingerprint: Any, ignore_info: tuple[str, ...] = ()) -> Any:
    if not isinstance(fingerprint, dict) or not fingerprint.get("ok"):
        return fingerprint
    pages = []
    for page in fingerprint.get("pages", []):
        contents = page.get("contents", {})
        pages.append(
            {
                "mediaBox": page.get("mediaBox"),
                "cropBox": page.get("cropBox"),
                "rotate": page.get("rotate"),
                "annots": page.get("annots"),
                "fonts": page.get("fonts"),
                "contentOperations": contents.get("operations"),
                "contentOperands": contents.get("operands"),
                "contentOperators": contents.get("operators"),
                "contentDecodeError": bool(contents.get("decodeError")),
            }
        )
    info = fingerprint.get("info") or {}
    if isinstance(info, dict):
        info = {key: value for key, value in info.items() if key not in ignore_info}
    catalog = fingerprint.get("catalog") or {}
    catalog_semantic = {key: catalog.get(key) for key in ("hasAcroForm", "hasOutlines", "hasNames", "hasMetadata", "hasPieceInfo")}
    return {
        "encrypted": fingerprint.get("encrypted"),
        "wasEncrypted": fingerprint.get("wasEncrypted"),
        "pageCount": fingerprint.get("pageCount"),
        "info": info,
        "catalog": catalog_semantic,
        "pages": pages,
    }


def structure_view(fingerprint: Any) -> Any:
    if not isinstance(fingerprint, dict) or not fingerprint.get("ok"):
        return fingerprint
    import copy

    view = copy.deepcopy(fingerprint)
    for page in view.get("pages", []):
        page.pop("id", None)
    return view


def numbers_differ(java: Any, rust: Any) -> bool:
    if isinstance(java, bool) or isinstance(rust, bool):
        return java != rust
    if isinstance(java, (int, float)) and isinstance(rust, (int, float)):
        return not math.isclose(float(java), float(rust), rel_tol=1e-6, abs_tol=1e-6)
    return False


def diff_json(java: Any, rust: Any, path: str = "") -> list[Diff]:
    if isinstance(java, dict) and isinstance(rust, dict):
        diffs: list[Diff] = []
        for key in sorted(set(java) | set(rust)):
            child = f"{path}.{key}" if path else str(key)
            if key not in java:
                diffs.append(Diff(child, "<missing>", rust[key]))
            elif key not in rust:
                diffs.append(Diff(child, java[key], "<missing>"))
            else:
                diffs.extend(diff_json(java[key], rust[key], child))
        return diffs
    if isinstance(java, list) and isinstance(rust, list):
        diffs = []
        if len(java) != len(rust):
            diffs.append(Diff(path or "<root>", len(java), len(rust)))
        for index, (left, right) in enumerate(zip(java, rust)):
            diffs.extend(diff_json(left, right, f"{path}[{index}]"))
        return diffs
    if java != rust and not (numbers_differ(java, rust) is False and java == rust):
        if numbers_differ(java, rust):
            return [Diff(path or "<root>", java, rust)]
        if java != rust:
            return [Diff(path or "<root>", java, rust)]
    return []


def split_known(case_name: str, diffs: list[Diff]) -> tuple[list[Diff], list[Diff]]:
    known: list[Diff] = []
    semantic: list[Diff] = []
    for diff in diffs:
        if any(endpoint in case_name and pattern.search(diff.path) for endpoint, pattern in KNOWN_DIFFS):
            known.append(diff)
        else:
            semantic.append(diff)
    return semantic, known


def content_type(response: Response) -> str:
    return response.headers.get("content-type", "").split(";", 1)[0].strip().lower()


def filename(response: Response) -> str:
    disposition = response.headers.get("content-disposition", "")
    match = re.search(r'filename="?([^";]+)"?', disposition)
    return match.group(1) if match else ""


def compare_pdf_outputs(case: Case, java_body: bytes, rust_body: bytes, cache: FingerprintCache) -> tuple[list[Diff], list[Diff]]:
    java_fp = cache.get(java_body)
    rust_fp = cache.get(rust_body)
    semantic = diff_json(semantic_view(java_fp, case.ignore_info), semantic_view(rust_fp, case.ignore_info))
    structure = diff_json(structure_view(java_fp), structure_view(rust_fp))
    return semantic, structure


def compare_zip_outputs(case: Case, java_body: bytes, rust_body: bytes, cache: FingerprintCache) -> tuple[list[Diff], list[Diff]]:
    semantic: list[Diff] = []
    structure: list[Diff] = []
    try:
        with zipfile.ZipFile(__import__("io").BytesIO(java_body)) as java_zip, zipfile.ZipFile(__import__("io").BytesIO(rust_body)) as rust_zip:
            java_names = [name for name in java_zip.namelist() if not name.endswith("/")]
            rust_names = [name for name in rust_zip.namelist() if not name.endswith("/")]
            if java_names != rust_names:
                semantic.append(Diff("zip.entries", java_names, rust_names))
            for name in sorted(set(java_names) & set(rust_names)):
                java_entry = java_zip.read(name)
                rust_entry = rust_zip.read(name)
                if not java_entry.startswith(b"%PDF") or not rust_entry.startswith(b"%PDF"):
                    if java_entry != rust_entry:
                        semantic.append(Diff(f"zip.{name}.bytes", len(java_entry), len(rust_entry)))
                    continue
                entry_semantic, entry_structure = compare_pdf_outputs(case, java_entry, rust_entry, cache)
                for diff in entry_semantic:
                    semantic.append(Diff(f"zip.{name}.{diff.path}", diff.java, diff.rust))
                for diff in entry_structure:
                    structure.append(Diff(f"zip.{name}.{diff.path}", diff.java, diff.rust))
    except zipfile.BadZipFile as error:
        semantic.append(Diff("zip.parse", "valid zip", str(error)))
    return semantic, structure


def compare_bodies(case: Case, java: Response, rust: Response, cache: FingerprintCache) -> tuple[list[Diff], list[Diff], str]:
    java_type = content_type(java)
    rust_type = content_type(rust)
    if java_type != rust_type:
        return [Diff("contentType", java_type, rust_type)], [], f"java={java_type} rust={rust_type}"
    if case.expect == "json" or java_type == "application/json":
        try:
            java_json = json.loads(java.body)
            rust_json = json.loads(rust.body)
        except json.JSONDecodeError as error:
            return [Diff("json.parse", "valid json", str(error))], [], "json"
        diffs = diff_json(java_json, rust_json)
        return diffs, [], "json"
    if case.expect == "zip" or java.body.startswith(b"PK"):
        return compare_zip_outputs(case, java.body, rust.body, cache) + ("zip",)
    if case.expect == "pdf" or java.body.startswith(b"%PDF"):
        semantic, structure = compare_pdf_outputs(case, java.body, rust.body, cache)
        return semantic, structure, "pdf"
    if java.body != rust.body:
        return [Diff("body.bytes", len(java.body), len(rust.body))], [], "bytes"
    return [], [], "bytes"


def run_case(case: Case, input_path: Path, java_url: str, rust_url: str, cache: FingerprintCache, timeout: float, java_headers: dict[str, str] | None = None) -> Result:
    data = input_path.read_bytes()
    files = [(FILE_FIELD, input_path.name, data)]
    fields = list(case.fields)
    java = post_multipart(java_url, case.path, files, fields, timeout, java_headers)
    rust = post_multipart(rust_url, case.path, files, fields, timeout)
    result = Result(case.name, input_path.name, java.status, rust.status)
    result.note = f"javaFile={filename(java)} rustFile={filename(rust)}"

    transport = [diff for diff in (Diff("java.transport", java.error, "") if java.status == 0 else None, Diff("rust.transport", "", rust.error) if rust.status == 0 else None) if diff]
    if transport:
        result.semantic.extend(transport)
        return result
    if java.status != rust.status:
        result.semantic.append(Diff("status", java.status, rust.status))
        if java.status >= 400 and rust.status >= 400:
            result.note += " both-error"
        return result
    if java.status >= 400:
        result.note += " both-error"
        return result

    semantic, structure, kind = compare_bodies(case, java, rust, cache)
    result.semantic, result.known = split_known(case.name, semantic)
    result.structure = structure
    result.note += f" {kind}"
    return result


def run_merge(inputs: list[Path], java_url: str, rust_url: str, cache: FingerprintCache, timeout: float, java_headers: dict[str, str] | None = None) -> Result:
    case = next(case for case in CASES if case.name == "general/merge-pdfs")
    first = inputs[0]
    second = inputs[1] if len(inputs) > 1 else inputs[0]
    files = [
        (FILE_FIELD, first.name, first.read_bytes()),
        (FILE_FIELD, second.name, second.read_bytes()),
    ]
    java = post_multipart(java_url, case.path, files, list(case.fields), timeout, java_headers)
    rust = post_multipart(rust_url, case.path, files, list(case.fields), timeout)
    result = Result(case.name, f"{first.name}+{second.name}", java.status, rust.status)
    if java.status == 0 or rust.status == 0:
        if java.status == 0:
            result.semantic.append(Diff("java.transport", java.error, ""))
        if rust.status == 0:
            result.semantic.append(Diff("rust.transport", "", rust.error))
        return result
    if java.status != rust.status:
        result.semantic.append(Diff("status", java.status, rust.status))
        return result
    if java.status >= 400:
        result.note = "both-error"
        return result
    semantic, structure, kind = compare_bodies(case, java, rust, cache)
    result.semantic, result.known = split_known(case.name, semantic)
    result.structure = structure
    result.note = kind
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description="Compare migrated Stirling PDF endpoints between Java and Rust.")
    parser.add_argument("--java-url", default=DEFAULT_JAVA)
    parser.add_argument("--rust-url", default=DEFAULT_RUST)
    parser.add_argument("--java-auth", default=os.environ.get("STIRLING_PARITY_JAVA_AUTH", ""), help="Bearer token for the Java backend (or set STIRLING_PARITY_JAVA_AUTH)")
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--fingerprint", default=DEFAULT_FINGERPRINT)
    parser.add_argument("--build-fingerprint", action="store_true")
    parser.add_argument("--include-testing", action="store_true")
    parser.add_argument("--case", action="append", default=[], help="substring filter for case names")
    parser.add_argument("--limit", type=int, default=0, help="limit number of input PDFs (0 = all)")
    parser.add_argument("--timeout", type=float, default=120.0)
    parser.add_argument("--strict-structure", action="store_true", help="fail on structural PDF diffs too")
    parser.add_argument("--fail-known", action="store_true", help="fail on diffs matching KNOWN_DIFFS")
    parser.add_argument("--out", type=Path, help="write JSON report")
    args = parser.parse_args()

    root = args.root.resolve()
    inputs = discover_inputs(root, args.include_testing)
    if args.limit > 0:
        inputs = inputs[: args.limit]
    if not inputs:
        print("no input PDFs found", file=sys.stderr)
        return 2

    cases = [case for case in CASES if not args.case or any(part in case.name for part in args.case)]
    if not cases:
        print("no cases selected", file=sys.stderr)
        return 2

    binary = ensure_fingerprint(root, args)
    cache = FingerprintCache(binary)
    java_headers = {"Authorization": f"Bearer {args.java_auth}"} if args.java_auth else None
    results: list[Result] = []

    for case in cases:
        if case.files == "pair":
            if inputs:
                results.append(run_merge(inputs, args.java_url, args.rust_url, cache, args.timeout, java_headers))
            continue
        for input_path in inputs:
            results.append(run_case(case, input_path, args.java_url, args.rust_url, cache, args.timeout, java_headers))

    failures = 0
    for result in results:
        structural_failures = len(result.structure) if args.strict_structure else 0
        known_failures = len(result.known) if args.fail_known else 0
        failed = result.failed or structural_failures or known_failures
        failures += int(failed)
        status = "FAIL" if failed else ("WARN" if result.known or result.structure else "PASS")
        print(
            f"{status:4} {result.case:38} {result.input_name:55} "
            f"java={result.java_status} rust={result.rust_status} "
            f"semantic={len(result.semantic)} known={len(result.known)} structure={len(result.structure)}"
        )
        for diff in (result.semantic + result.known)[:8]:
            print(f"      {diff.kind}: {diff.path}: java={diff.java!r} rust={diff.rust!r}")
        if len(result.semantic) + len(result.known) > 8:
            print(f"      ... {len(result.semantic) + len(result.known) - 8} more semantic/known diffs")
        if args.strict_structure:
            for diff in result.structure[:8]:
                print(f"      structure: {diff.path}: java={diff.java!r} rust={diff.rust!r}")
            if len(result.structure) > 8:
                print(f"      ... {len(result.structure) - 8} more structural diffs")

    summary = {
        "inputs": [str(path) for path in inputs],
        "cases": [case.name for case in cases],
        "results": [
            {
                "case": result.case,
                "input": result.input_name,
                "javaStatus": result.java_status,
                "rustStatus": result.rust_status,
                "semanticDiffs": [vars(diff) | {"kind": diff.kind} for diff in result.semantic],
                "knownDiffs": [vars(diff) | {"kind": diff.kind} for diff in result.known],
                "structureDiffCount": len(result.structure),
                "note": result.note,
            }
            for result in results
        ],
        "failures": failures,
    }
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(summary, indent=2))
    print(f"\n{len(results)} comparisons, {failures} failures")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
