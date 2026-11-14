#!/bin/sh
# CPython pinned from python.org, compiled on the runner's libc (including IBM UBI).
set -eu
prefix="${1:?installation prefix required}"
release=3.14.6
sha256=143b1dddefaec3bd2e21e3b839b34a2b7fb9842272883c576420d605e9f30c63
build_dir="$(mktemp -d)"
trap 'rm -rf "$build_dir"' EXIT
curl --fail --location --retry 3 "https://www.python.org/ftp/python/$release/Python-$release.tar.xz" -o "$build_dir/python.tar.xz"
printf '%s  %s\n' "$sha256" "$build_dir/python.tar.xz" | sha256sum --check -
tar -xJf "$build_dir/python.tar.xz" -C "$build_dir"
cd "$build_dir/Python-$release"
./configure --prefix="$prefix" --enable-shared --with-ensurepip=install
make -j2
make install
LD_LIBRARY_PATH="$prefix/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" "$prefix/bin/python3.14" -c 'import ssl, sqlite3, bz2, lzma, ctypes, venv; import sys; assert sys.version_info[:2] == (3, 14)'
