#!/bin/sh
set -eu

image="yaat-codex-integration:0.146.0"

docker build \
  --file docker/codex-integration.Dockerfile \
  --tag "$image" \
  .

docker run \
  --rm \
  --network none \
  --cap-drop ALL \
  --security-opt no-new-privileges \
  --tmpfs /tmp:rw,exec,nosuid,nodev,size=512m \
  "$image"
