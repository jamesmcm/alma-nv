#!/bin/bash
set -e

if ! docker info >/dev/null 2>&1; then
    echo "ERROR: Cannot access Docker. Make sure:"
    echo "  1. Docker is running"
    echo "  2. You're in the 'docker' group, or"
    echo "  3. Run with sudo"
    exit 1
fi

echo "WARNING: ALMA will run with privileged access to devices."
echo "This is required for disk operations but has security implications."

if ! docker image inspect alma-nv >/dev/null 2>&1; then
    echo "Building ALMA Docker image..."
    docker build -t alma-nv .
fi

exec docker run --rm -it \
    --privileged \
    -v /dev:/dev:rw \
    -v /sys:/sys:ro \
    -v "$(pwd)":/work \
    alma-nv "$@"
