#!/usr/bin/env python3
"""验证原生扩展与安装来源，避免误用 PyPI 的 Nautilus wheel。"""
from importlib import metadata
import json
from pathlib import Path
import sys
import tomllib

from nautilus_trader.core import nautilus_pyo3

distribution = metadata.distribution("nautilus_trader")
upstream = Path(__file__).resolve().parent.parent / "vendor/nautilus_trader/pyproject.toml"
expected_version = tomllib.loads(upstream.read_text())["project"]["version"]
assert distribution.version == expected_version
direct = json.loads(distribution.read_text("direct_url.json"))
assert direct["url"].startswith("file://"), direct
assert ".whl" in direct["url"], direct
extension = Path(nautilus_pyo3.__file__).resolve()
assert extension.suffix == ".so" and extension.is_relative_to(Path(sys.prefix).resolve())
print(f"SOURCE_WHEEL={direct['url']}")
print(f"NATIVE_EXTENSION={extension}")
print(f"NAUTILUS_VERSION={distribution.version}")
