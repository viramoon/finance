#!/usr/bin/env bash
set -Eeuo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
# shellcheck source=../toolchain.env
source "$repo/toolchain.env"

dry_run=false
(( $# <= 1 )) || { echo "Expected at most one argument" >&2; exit 2; }
case "${1:-}" in
  "") ;;
  --dry-run) dry_run=true ;;
  --help|-h)
    echo "Usage: bash scripts/bootstrap-research.sh [--dry-run]"
    echo "Set FINANCE_RESEARCH_TOOLCHAIN_ROOT to choose the toolchain directory."
    exit 0
    ;;
  *) echo "Expected --dry-run or --help" >&2; exit 2 ;;
esac

root=${FINANCE_RESEARCH_TOOLCHAIN_ROOT:-$HOME/.local/share/finance-research}
export FINANCE_BUILD_ROOT=${FINANCE_BUILD_ROOT:-$root/build}
if [[ "$root" != /* || "$FINANCE_BUILD_ROOT" != /* ]]; then
  echo "Toolchain and build directories must be absolute" >&2
  exit 2
fi

packages=(
  build-essential bison flex pkg-config libssl-dev zlib1g-dev libtinfo6 libxml2 libz3-4
  ca-certificates curl git tar gzip xz-utils
)
env_file="$root/env.sh"
if "$dry_run"; then
  printf 'Toolchain: rust=%s rustup=%s uv=%s python=%s llvm=%s\n' \
    "$RUST_VERSION" "$RUSTUP_VERSION" "$UV_VERSION" "$PYTHON_VERSION" "$LLVM_VERSION"
  printf 'System packages: %s\n' "${packages[*]}"
  printf 'Toolchain env: %s\nBuild root: %s\n' "$env_file" "$FINANCE_BUILD_ROOT"
  exit 0
fi

if [[ "$(uname -s)" != Linux || "$(uname -m)" != x86_64 ]]; then
  echo "The research bootstrap requires Ubuntu 24.04 on Linux x86_64" >&2
  exit 2
fi

apt=(apt-get -o Acquire::Retries=5)
if (( EUID != 0 )); then
  apt=(sudo "${apt[@]}")
fi
"${apt[@]}" update
"${apt[@]}" install -y --no-install-recommends "${packages[@]}"

export RUSTUP_HOME="$root/rustup" CARGO_HOME="$root/cargo"
export UV_PYTHON_INSTALL_DIR="$root/python" UV_PYTHON_PREFERENCE=only-managed
export UV_CACHE_DIR="$FINANCE_BUILD_ROOT/uv-cache"
downloads="$root/downloads"
uv_dir="$root/uv-$UV_VERSION"
bin="$root/bin"
mkdir -p "$downloads" "$uv_dir" "$bin"

download() {
  local url=$1 destination=$2
  if [[ ! -s "$destination" ]]; then
    curl --fail --location --retry 5 --retry-all-errors --connect-timeout 20 --max-time 1800 \
      "$url" -o "$destination.part"
    mv -- "$destination.part" "$destination"
  fi
}

rustup="$CARGO_HOME/bin/rustup"
if [[ ! -x "$rustup" ]]; then
  mkdir -p "$downloads/rustup-$RUSTUP_VERSION"
  installer="$downloads/rustup-$RUSTUP_VERSION/rustup-init"
  download "https://static.rust-lang.org/rustup/archive/$RUSTUP_VERSION/x86_64-unknown-linux-gnu/rustup-init" "$installer"
  chmod +x "$installer"
  "$installer" -y --no-modify-path --profile minimal --default-toolchain "$RUST_VERSION"
elif ! "$rustup" run "$RUST_VERSION" rustc --version >/dev/null 2>&1; then
  "$rustup" toolchain install "$RUST_VERSION" --profile minimal
fi

uv="$uv_dir/uv"
if [[ ! -x "$uv" ]]; then
  archive="$downloads/uv-$UV_VERSION.tar.gz"
  download "https://github.com/astral-sh/uv/releases/download/$UV_VERSION/uv-x86_64-unknown-linux-gnu.tar.gz" "$archive"
  tar -xzf "$archive" --strip-components=1 -C "$uv_dir"
fi
if ! "$uv" python find --no-project --system --managed-python "$PYTHON_VERSION" >/dev/null 2>&1; then
  "$uv" python install --no-bin "$PYTHON_VERSION"
fi
python=$("$uv" python find --no-project --system --managed-python "$PYTHON_VERSION")
ln -sfn -- "$python" "$bin/python3"
ln -sfn -- "$python" "$bin/python"
bash "$repo/scripts/setup-llvm.sh"
llvm="$FINANCE_BUILD_ROOT/tools/llvm-$LLVM_VERSION"

# Publish only after the complete toolchain is available.
tmp_env=$(mktemp "$root/env.sh.XXXXXX")
trap 'rm -f -- "$tmp_env"' EXIT
{
  for name in FINANCE_BUILD_ROOT RUSTUP_HOME CARGO_HOME UV_PYTHON_INSTALL_DIR UV_PYTHON_PREFERENCE UV_CACHE_DIR; do
    printf 'export %s=%q\n' "$name" "${!name}"
  done
  printf 'export FINANCE_TOOLCHAIN_ENV=%q\n' "$env_file"
  printf 'export LIBCLANG_PATH=%q\n' "$llvm/lib"
  printf 'export PATH=%q:"$PATH"\n' "$uv_dir:$CARGO_HOME/bin:$bin:$llvm/bin"
  printf 'export LD_LIBRARY_PATH=%q"${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"\n' "$llvm/lib"
} > "$tmp_env"
mv -- "$tmp_env" "$env_file"
printf 'source %q\n' "$env_file"
