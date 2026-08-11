#!/bin/sh
set -eu

image="yaat-claude-integration:2.1.226"

docker build \
  --file docker/claude-integration.Dockerfile \
  --tag "$image" \
  .

docker run --rm \
  --network none \
  --cap-drop ALL \
  --security-opt no-new-privileges \
  --env CLAUDE_CONFIG_DIR=/tmp/yaat-claude-source \
  "$image"
