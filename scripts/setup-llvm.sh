#!/usr/bin/env bash
set -Eeuo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
source "$repo/toolchain.env"
root=${FINANCE_BUILD_ROOT:-/home/box/fertile/cache/finance}
llvm="$root/tools/llvm-$LLVM_VERSION"
if [[ -x "$llvm/bin/clang" ]] &&
   [[ "$(LD_LIBRARY_PATH="$llvm/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" "$llvm/bin/clang" --version)" = *"clang version $LLVM_VERSION"* ]]; then
  echo "LLVM_CACHE_HIT=true"; exit
fi
mkdir -p "$root/downloads" "$llvm"
archive="$root/downloads/llvm-$LLVM_VERSION.tar.xz"
if [[ ! -s "$archive" ]]; then
  curl --fail --location --retry 5 --retry-all-errors --connect-timeout 20 --max-time 1800 \
    "https://github.com/llvm/llvm-project/releases/download/llvmorg-$LLVM_VERSION/LLVM-$LLVM_VERSION-Linux-X64.tar.xz" -o "$archive.part"
  mv "$archive.part" "$archive"
fi
tar -xJf "$archive" --strip-components=1 -C "$llvm" --wildcards \
  '*/bin/clang' '*/bin/clang++' "*/bin/clang-${LLVM_VERSION%%.*}" '*/bin/lld' '*/bin/ld.lld' '*/lib/lib*.so*' '*/lib/clang/*' '*/include/clang-c/*'
echo "LLVM_CACHE_HIT=false"
