#!/usr/bin/env bash
# 唯一入口：锁定依赖、构建本地源码 wheel、校验来源；不从索引安装 Nautilus wheel。
set -Eeuo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo"
if [[ -f "${FERTILE_BOX_HOME:-/home/box}/fertile/env.sh" ]]; then
  source "${FERTILE_BOX_HOME:-/home/box}/fertile/env.sh"
fi
source "$repo/toolchain.env"
root=${FINANCE_BUILD_ROOT:-/home/box/fertile/cache/finance}
export FINANCE_BUILD_ROOT="$root"
[[ "$root" = /* ]] || { echo "FINANCE_BUILD_ROOT 必须是绝对路径" >&2; exit 2; }
vendor="$repo/vendor/nautilus_trader"
[[ -f "$vendor/pyproject.toml" ]] || { echo "先 git submodule update --init --recursive" >&2; exit 1; }
sha=$(git -C "$vendor" rev-parse HEAD)
[[ "$sha" = "$NAUTILUS_COMMIT" ]] || { echo "Nautilus 指针与生成的工具链元数据不同，请先同步版本。" >&2; exit 1; }
[[ -z "$(git -C "$vendor" status --porcelain --untracked-files=no)" ]] ||
  { echo "禁止直接在 vendor 修改源码；先在镜像提交补丁。" >&2; exit 1; }
# 上游 build.py 只接受 stable/nightly 字面值；不向它传数字版本。
# 实际 cargo 仍由已锁定的 rust-toolchain.toml 和 rustup 默认版本选择。
unset RUSTUP_TOOLCHAIN
[[ "$(rustc --version)" = "rustc $RUST_VERSION "* ]]
[[ "$(uv --version)" = "uv $UV_VERSION"* ]]
[[ "$(python3 --version)" = "Python $PYTHON_VERSION" ]]
if ! command -v clang >/dev/null ||
   [[ "$(clang --version)" != *"clang version $LLVM_VERSION"* ]]; then
  bash "$repo/scripts/setup-llvm.sh"
  export PATH="$root/tools/llvm-$LLVM_VERSION/bin:$PATH"
  export LIBCLANG_PATH="$root/tools/llvm-$LLVM_VERSION/lib"
  export LD_LIBRARY_PATH="$LIBCLANG_PATH${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi
[[ "$(clang --version)" = *"clang version $LLVM_VERSION"* ]]
command -v ld.lld >/dev/null
export BUILD_MODE=${BUILD_MODE:-ci-pr} HIGH_PRECISION=true
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2} PARALLEL_BUILD=false
# Cargo 全部编译输出及 uv 环境 / 下载都持久化，vendor 保持干净。
export CARGO_TARGET_DIR="$root/cargo-target"
export UV_CACHE_DIR="$root/uv-cache"
export UV_PROJECT_ENVIRONMENT="$root/venv"
mkdir -p "$root/wheels" "$root/logs"
key=$(
  {
    printf '%s\n' "$sha" "$BUILD_MODE" "$HIGH_PRECISION" "$(uname -m)"
    rustc -Vv
    clang --version
    python3 -c 'import sysconfig; print(sysconfig.get_config_var("SOABI"))'
    sha256sum toolchain.env uv.lock scripts/build.sh
  } | sha256sum | cut -d ' ' -f 1
)
wheel_dir="$root/wheels/$key"
start=$SECONDS
uv sync --frozen --python "$PYTHON_VERSION" --group build --no-install-package nautilus-trader --inexact
venv="$UV_PROJECT_ENVIRONMENT"
export PYO3_PYTHON="$venv/bin/python"
python_lib=$("$venv/bin/python" -c 'import sysconfig; print(sysconfig.get_config_var("LIBDIR"))')
export LD_LIBRARY_PATH="$python_lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
if [[ -f "$wheel_dir/SHA256SUMS" ]]; then
  (cd "$wheel_dir"; sha256sum --check SHA256SUMS)
  echo "BUILD_CACHE_HIT=true"
else
  echo "BUILD_CACHE_HIT=false"
  source_dir="$root/sources/$key"
  mkdir -p "$root/sources" "$wheel_dir"
  if [[ ! -f "$source_dir/.git" ]]; then
    git -C "$vendor" worktree prune
    git -C "$vendor" worktree add --detach "$source_dir" "$sha"
  fi
  # 显式传本地源码目录。运行上游 PEP 517 backend；产物在隔离 worktree 生成。
  uv build --python "$venv/bin/python" --no-build-isolation --wheel \
    --out-dir "$wheel_dir" "$source_dir" 2>&1 | tee "$root/logs/build-$key.log"
  (cd "$wheel_dir"; sha256sum ./*.whl > SHA256SUMS)
  printf '%s\n' "$sha" > "$wheel_dir/source-commit"
fi
[[ "$(cat "$wheel_dir/source-commit")" = "$sha" ]]
wheels=("$wheel_dir"/*.whl)
(( ${#wheels[@]} == 1 )) || { echo "wheel 数量异常" >&2; exit 1; }
uv pip install --python "$venv/bin/python" --no-deps --reinstall "${wheels[0]}"
"$venv/bin/python" "$repo/scripts/verify-source.py"
echo "BUILD_SECONDS=$((SECONDS-start))"
echo "SOURCE_COMMIT=$sha"
echo "FINANCE_PYTHON=$venv/bin/python"
