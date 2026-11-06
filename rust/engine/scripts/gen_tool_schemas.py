#!/usr/bin/env python3
"""Emit JSON schemas for every tool parameter model into
`src/models/tool_schemas.json`, consumed by `src/models/tool_schemas.rs`.

`tool_models.py` only needs pydantic, but the `stirling` package __init__
pulls in FastAPI, so the modules are loaded by path with a stub package —
no engine dependencies required beyond pydantic:

    uv venv /tmp/eng-venv --python $(which python3)
    uv pip install --python /tmp/eng-venv/bin/python pydantic
    /tmp/eng-venv/bin/python scripts/gen_tool_schemas.py
"""

from __future__ import annotations

import importlib.util
import json
import sys
import types
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
ENGINE_SRC = ROOT / "engine" / "src"
OUT = Path(__file__).resolve().parents[1] / "src" / "models" / "tool_schemas.json"


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def main() -> None:
    # Stub the package chain so `stirling/__init__.py` (which imports the
    # FastAPI app) never executes.
    stirling = types.ModuleType("stirling")
    stirling.__path__ = []
    models_pkg = types.ModuleType("stirling.models")
    models_pkg.__path__ = []
    sys.modules["stirling"] = stirling
    sys.modules["stirling.models"] = models_pkg

    _load("stirling.models.base", ENGINE_SRC / "stirling" / "models" / "base.py")
    tool_models = _load(
        "stirling.models.tool_models",
        ENGINE_SRC / "stirling" / "models" / "tool_models.py",
    )

    schemas = {
        endpoint.value: tool_models.OPERATIONS[endpoint].model_json_schema()
        for endpoint in tool_models.OPERATIONS
    }
    OUT.write_text(json.dumps(schemas, indent=1, sort_keys=True) + "\n")
    print(f"wrote {OUT} ({len(schemas)} schemas)")


if __name__ == "__main__":
    main()
