#!/bin/sh
set -eu
# Pass lib/ from the v1.5.7 Zstandard source tree or vendored zstd-sys package.
zstd_lib=$1
asset_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
zig cc -target wasm32-wasi -mexec-model=reactor -Oz -DZSTD_DISABLE_ASM=1 \
    -DZSTD_LEGACY_SUPPORT=0 -DZSTD_STRIP_ERROR_STRINGS -I"$zstd_lib" \
    -Wl,--strip-all -Wl,-z,stack-size=262144 -Wl,--max-memory=16777216 \
    -Wl,--export=ratio_input -Wl,--export=ratio_output \
    -Wl,--export=ratio_decode -Wl,--export=ratio_set_dictionary \
    "$asset_dir/dashboard-decoder.c" "$zstd_lib"/common/*.c "$zstd_lib"/decompress/*.c \
    -o "$asset_dir/dashboard-decoder.wasm"
