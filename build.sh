#!/usr/bin/env bash
set -e

export PATH="/nix/store/hb2bs5fg5wkm04x565737qd5nh2hy5nk-gcc-wrapper-15.2.0/bin:/nix/store/m0rbbdfsbkdqpr6bs621jwi21ra1br4g-binutils-wrapper-2.44/bin:/nix/store/mwygqdwl6ysbw4aygi0hy3x0si09nkqn-mold-unwrapped-wrapper-2.40.4/bin:/nix/store/yqcsaywfvcyy9wmbzb5fawp29icgi7cb-cargo-1.92.0/bin:/nix/store/d0immrndpxnhx32vcfbxngmnf7hmqyc3-pkg-config-wrapper-0.29.2/bin:$PATH"
export LIBRARY_PATH="$(find /nix/store -maxdepth 4 -name "*.so" -exec dirname {} \; 2>/dev/null | sort -u | tr '\n' ':')"
export PKG_CONFIG_PATH="\
/nix/store/v38m4px5x7b39fsbcnxbi1l7n5k8a23k-systemd-259.3-dev/lib/pkgconfig:\
/nix/store/2wm927kh3f3xv1406s27wm22wz13wv8a-libdisplay-info-0.3.0/lib/pkgconfig:\
/nix/store/kk77qk00dg4an1a3lcdd6i32q3jb7gpp-seatd-0.9.3-dev/lib/pkgconfig:\
/nix/store/zj092hkwgq72k5vhqnq1wfgw3jhgi3vi-libinput-1.29.2-dev/lib/pkgconfig:\
/nix/store/28sadwrjw8vpr7hk2rv52j24fh5m6961-mesa-libgbm-25.1.0/lib/pkgconfig:\
/nix/store/nm8vpqbxi7qx1dgq5nb4arj0hqhbxvby-libdrm-2.4.131-dev/lib/pkgconfig"

cd "$(dirname "$0")"
cargo build --release --target-dir /mnt/dawn-build/target "$@"

echo ""
echo "Binary: /mnt/dawn-build/target/release/dawn"
