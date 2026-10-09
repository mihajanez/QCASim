#!/usr/bin/env bash
# Installs qca-sim for the Slurm scripts (run it on the FRIDA login node).
#
#   install.sh [--version vX.Y.Z] [--prefix DIR]   download the static release
#                                                  binaries (x86_64 and aarch64)
#   install.sh --build [--partition P]             build from this source tree
#                                                  in a Rust container (dev
#                                                  partition, ~5 min)
#   install.sh --image [--version vX.Y.Z] [--to F] import the container image
#                                                  into a squashfs file for
#                                                  QCASIM_IMAGE
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=qcasim-env.sh
source "$HERE/qcasim-env.sh"

VERSION=latest
MODE=download
PARTITION=dev
IMAGE_FILE=""
while [[ $# -gt 0 ]]; do
    case $1 in
        --version) VERSION=$2; shift 2 ;;
        --prefix) QCASIM_PREFIX=$2; shift 2 ;;
        --build) MODE=build; shift ;;
        --image) MODE=image; shift ;;
        --to) IMAGE_FILE=$2; shift 2 ;;
        --partition) PARTITION=$2; shift 2 ;;
        -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
done
BIN_DIR=$QCASIM_PREFIX/bin
mkdir -p "$BIN_DIR"

case $MODE in
download)
    if [[ $VERSION == latest ]]; then
        BASE=https://github.com/$QCASIM_REPO/releases/latest/download
    else
        BASE=https://github.com/$QCASIM_REPO/releases/download/$VERSION
    fi
    for arch in x86_64 aarch64; do
        echo "downloading qca-sim-linux-$arch ($VERSION)"
        curl -fL --retry 3 -o "$BIN_DIR/qca-sim-$arch.tmp" "$BASE/qca-sim-linux-$arch"
        chmod +x "$BIN_DIR/qca-sim-$arch.tmp"
        mv "$BIN_DIR/qca-sim-$arch.tmp" "$BIN_DIR/qca-sim-$arch"
    done
    ;;
build)
    SRC=$(cd "$HERE/../.." && pwd)
    [[ -f $SRC/Cargo.toml ]] || { echo "--build needs a QCASim source tree around $HERE" >&2; exit 1; }
    arch=$(uname -m)
    echo "building qca-sim from $SRC on partition $PARTITION"
    # Pyxis/Enroot: the official Rust image (Alpine, so the binary is static).
    srun -p "$PARTITION" -c 8 --mem=16G -t 00:45:00 \
        --container-image=rust:1-alpine \
        --container-mounts="$SRC:$SRC" --container-workdir="$SRC" \
        sh -c 'apk add --no-cache musl-dev >/dev/null && CARGO_TARGET_DIR=target/frida cargo build --release --locked -p qca-sim'
    install -m 755 "$SRC/target/frida/release/qca-sim" "$BIN_DIR/qca-sim-$arch"
    ;;
image)
    TAG=${VERSION#v}
    [[ $VERSION == latest ]] && TAG=latest
    IMAGE_FILE=${IMAGE_FILE:-${WORK:-$HOME}/images/qcasim-$TAG.sqsh}
    mkdir -p "$(dirname "$IMAGE_FILE")"
    echo "importing ghcr.io/$QCASIM_REPO:$TAG into $IMAGE_FILE"
    srun -p "$PARTITION" -c 2 --mem=8G -t 00:20:00 \
        --container-image="ghcr.io#${QCASIM_REPO,,}:$TAG" \
        --container-save="$IMAGE_FILE" true
    echo "use it with: export QCASIM_IMAGE=$IMAGE_FILE"
    exit 0
    ;;
esac

for arch in x86_64 aarch64; do
    [[ -x $BIN_DIR/qca-sim-$arch ]] && echo "installed $BIN_DIR/qca-sim-$arch"
done
QCASIM_BIN="" qcasim_local --version
