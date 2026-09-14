#!/bin/sh
# Maps Docker's TARGETPLATFORM to a Rust target triple and, when cross
# compiling, installs the matching GNU cross toolchain. Writes the settings
# cargo needs to /etc/rust-target.env so the build step can `. ` it.
set -eu

target="${TARGETPLATFORM:-linux/amd64}"
build="${BUILDPLATFORM:-linux/amd64}"
# normalise "linux/arm64/v8" -> "linux/arm64"
target="${target%/v8}"
build="${build%/v8}"

case "$target" in
  linux/386)    RUST_TARGET=i686-unknown-linux-gnu;        GNU=i686-linux-gnu;     LIBC=libc6-dev-i386-cross ;;
  linux/amd64)  RUST_TARGET=x86_64-unknown-linux-gnu;      GNU=x86_64-linux-gnu;   LIBC=libc6-dev-amd64-cross ;;
  linux/arm64)  RUST_TARGET=aarch64-unknown-linux-gnu;     GNU=aarch64-linux-gnu;  LIBC=libc6-dev-arm64-cross ;;
  linux/arm/v7) RUST_TARGET=armv7-unknown-linux-gnueabihf; GNU=arm-linux-gnueabihf; LIBC=libc6-dev-armhf-cross ;;
  *) echo "unsupported TARGETPLATFORM: $target" >&2; exit 1 ;;
esac

rustup target add "$RUST_TARGET"
echo "export RUST_TARGET=$RUST_TARGET" > /etc/rust-target.env

if [ "$target" != "$build" ]; then
  apt-get update
  apt-get install -y --no-install-recommends "gcc-$GNU" "$LIBC"
  rm -rf /var/lib/apt/lists/*
  upper=$(echo "$RUST_TARGET" | tr 'a-z-' 'A-Z_')
  upper_lower=$(echo "$RUST_TARGET" | tr '-' '_')
  {
    echo "export CARGO_TARGET_${upper}_LINKER=${GNU}-gcc"
    echo "export CC_${upper_lower}=${GNU}-gcc"
  } >> /etc/rust-target.env
fi

cat /etc/rust-target.env
