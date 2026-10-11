#!/usr/bin/env bash
# Builds G2 (guests/py): CPython 3.12 embedded in a WASI reactor.
#
# Uses wasi-sdk 20 and VMware Labs' prebuilt libpython for it (the static
# library is built against that wasi-libc). Both are downloaded once into
# $FORKASM_TOOLCHAIN (default ~/.cache/forkasm-toolchain) and checksummed.
#
# Output: guests/py/out/py.wasm and guests/py/out/lib/ (the stdlib, mounted
# read-only at /usr/local/lib in the guest).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
tc="${FORKASM_TOOLCHAIN:-$HOME/.cache/forkasm-toolchain}"
sdk="$tc/wasi-sdk-20.0"
py="$tc/libpython"

sdk_url=https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-20/wasi-sdk-20.0-linux.tar.gz
sdk_sha=7030139d495a19fbeccb9449150c2b1531e15d8fb74419872a719a7580aad0f9
py_url=https://github.com/vmware-labs/webassembly-language-runtimes/releases/download/python%2F3.12.0%2B20231211-040d5a6/libpython-3.12.0-wasi-sdk-20.0.tar.gz
py_sha=f46bc497afbebc90966f3bc03a0de9697338227cc94a99abf8f1bebd2a7f087a

fetch() { # url sha256 dest-dir
    local file="$tc/dl/$(basename "${1//%2F//}")"
    mkdir -p "$tc/dl" "$3"
    [ -f "$file" ] || curl -fsSL -o "$file" "$1"
    echo "$2  $file" | sha256sum -c --quiet
    tar -xzf "$file" -C "$3"
}
[ -x "$sdk/bin/clang" ] || fetch "$sdk_url" "$sdk_sha" "$tc"
[ -f "$py/lib/wasm32-wasi/libpython3.12.a" ] || fetch "$py_url" "$py_sha" "$py"

out="$here/out"
mkdir -p "$out"
"$sdk/bin/clang" --target=wasm32-wasi --sysroot="$sdk/share/wasi-sysroot" \
    -mexec-model=reactor -O2 \
    -I"$py/include/python3.12" "$here/driver.c" \
    -L"$py/lib/wasm32-wasi" -lpython3.12 \
    -lwasi-emulated-getpid -lwasi-emulated-signal -lwasi-emulated-process-clocks \
    -Wl,-z,stack-size=524288 -Wl,--stack-first -Wl,--strip-all \
    -o "$out/py.wasm"
rm -rf "$out/lib"
cp -r "$py/usr/local/lib" "$out/lib"
ls -la "$out/py.wasm"
