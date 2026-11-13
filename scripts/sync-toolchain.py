#!/usr/bin/env python3
"""从 fertile 的唯一维护源生成本仓库版本文件；不从网络读取凭据。"""
import argparse
from pathlib import Path
import re

parser = argparse.ArgumentParser()
parser.add_argument("source", type=Path, help="fertile/deploy/toolchain.env")
parser.add_argument("--check", action="store_true")
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
content = args.source.read_text()
values = dict(re.findall(r"^([A-Z_]+)=([^\n]+)$", content, re.M))
outputs = {
    "toolchain.env": "# GENERATED：源为 fertile/deploy/toolchain.env；运行 scripts/sync-toolchain.py 更新。\n" + content,
    "rust-toolchain.toml": '[toolchain]\nchannel = "' + values["RUST_VERSION"] + '"\nprofile = "minimal"\n',
    ".python-version": values["PYTHON_VERSION"] + "\n",
    ".uv-version": values["UV_VERSION"] + "\n",
}
project = root / "pyproject.toml"
outputs["pyproject.toml"] = re.sub(
    r'^required-version = "==[^"]+"$',
    'required-version = "==' + values["UV_VERSION"] + '"',
    project.read_text(), flags=re.M,
)
for name, expected in outputs.items():
    path = root / name
    if args.check:
        if not path.exists() or path.read_text() != expected:
            raise SystemExit(f"版本文件未同步：{name}")
    else:
        path.write_text(expected)
print("PASS toolchain files " + ("match canonical source" if args.check else "generated"))
