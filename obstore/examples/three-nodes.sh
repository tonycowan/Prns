#!/usr/bin/env bash
# Recreate the three-node example described in obstore/OBJECT-SERVICES.md.
#
# A is the LOA and the release origin. It holds the LOA private key and public
# key in loa/local, and the same public key in loa/trusted.
# B and C hold only that public key in loa/trusted. Their loa/local directories
# are empty.
#
# A listens on 127.0.0.1:48001. B connects to A and listens on 127.0.0.1:48002.
# C connects to B. share_instance is No, so the three prnsd processes stay
# separate. enable_transport is Yes, so B can forward between A and C.
# B does not fetch. C auto-stages firmware for board heltec-v4.
#
# Usage:
#   obstore/examples/three-nodes.sh [DEST]
#
# DEST defaults to ./example-nodes. The directory must not already exist.
# Set THREE_NODES_BASE_PORT to move the TCP line off 48001.
# Set OBJECT_SERVICES to an already-built object-services binary to skip cargo.
#
# The script writes configs and credentials. It does not start prnsd.
# The generated private key is a new LOD secret. Do not commit DEST.

set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
dest=${1:-"$PWD/example-nodes"}
base_port=${THREE_NODES_BASE_PORT:-48001}

if [[ ! "$base_port" =~ ^[0-9]+$ ]] || (( base_port < 1 || base_port > 65534 )); then
    echo "three-nodes: THREE_NODES_BASE_PORT must be a port from 1 to 65534" >&2
    exit 1
fi
port_ab=$base_port
port_bc=$((base_port + 1))

if [[ -e "$dest" ]]; then
    echo "three-nodes: destination already exists: $dest" >&2
    exit 1
fi

mkdir -p "$dest/a" "$dest/b" "$dest/c"

reticulum='[reticulum]
  enable_transport = Yes
  share_instance = No
  panic_on_interface_error = No

[logging]
  loglevel = 4
  logtimestamps = Yes'

cat > "$dest/a/config" <<EOF
# Node A. LOA for this LOD, and the origin of the release example.
$reticulum

[interfaces]
  [[AB]]
    type = TCPServerInterface
    enabled = Yes
    listen_ip = 127.0.0.1
    listen_port = $port_ab

[object-services]
  [[object-service]]
    object-store-directory = object-store
    cdn = No
    auto-stage = No
    auto-update = No

  [[object-transfer]]
    object-transfer-directory = object-transfer
    minimum-bytes-per-second = 0
EOF

cat > "$dest/b/config" <<EOF
# Node B. Middle hop. Forwards releases and does not fetch.
$reticulum

[interfaces]
  [[AB]]
    type = TCPClientInterface
    enabled = Yes
    target_host = 127.0.0.1
    target_port = $port_ab

  [[BC]]
    type = TCPServerInterface
    enabled = Yes
    listen_ip = 127.0.0.1
    listen_port = $port_bc

[object-services]
  [[object-service]]
    object-store-directory = object-store
    cdn = No
    auto-stage = No
    auto-update = No

  [[object-transfer]]
    object-transfer-directory = object-transfer
    minimum-bytes-per-second = 0
EOF

cat > "$dest/c/config" <<EOF
# Node C. Fetches auto-stage firmware for board heltec-v4.
$reticulum

[interfaces]
  [[BC]]
    type = TCPClientInterface
    enabled = Yes
    target_host = 127.0.0.1
    target_port = $port_bc

[object-services]
  [[object-service]]
    object-store-directory = object-store
    cdn = No
    auto-stage = Yes
    auto-update = No
    board = heltec-v4

  [[object-transfer]]
    object-transfer-directory = object-transfer
    minimum-bytes-per-second = 0
EOF

run_object_services() {
    if [[ -n "${OBJECT_SERVICES:-}" ]]; then
        "$OBJECT_SERVICES" "$@"
    else
        cargo run --quiet --locked --manifest-path "$repo_root/obstore/Cargo.toml" --bin object-services -- "$@"
    fi
}

public_key=$(run_object_services create-lod --config "$dest/a" --offline)
run_object_services trust-loa --config "$dest/b" --public-key "$public_key"
run_object_services trust-loa --config "$dest/c" --public-key "$public_key"

test -f "$dest/a/object-store/loa/local/private"
test -f "$dest/a/object-store/loa/local/public"
test -f "$dest/a/object-store/loa/trusted/$public_key"
test ! -e "$dest/b/object-store/loa/local/private"
test -d "$dest/b/object-store/loa/local"
test -f "$dest/b/object-store/loa/trusted/$public_key"
test ! -e "$dest/c/object-store/loa/local/private"
test -d "$dest/c/object-store/loa/local"
test -f "$dest/c/object-store/loa/trusted/$public_key"

cat <<EOF
LOD public key: $public_key

A  $dest/a   loa/local private+public, loa/trusted/$public_key
B  $dest/b   loa/trusted/$public_key
C  $dest/c   loa/trusted/$public_key

TCP line: A listens on 127.0.0.1:$port_ab, B listens on 127.0.0.1:$port_bc.

Start A, then B, then C:
  prnsd run --config $dest/a
  prnsd run --config $dest/b
  prnsd run --config $dest/c

Browse A's store:
  cargo run --locked --manifest-path $repo_root/obstore/browser/Cargo.toml -- $dest/a/object-store
EOF
