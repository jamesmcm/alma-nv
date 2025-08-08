#!/bin/bash
set -e

echo "ALMA Docker Container Starting..."

# Allow help and version commands to run without privileged access
if [[ "$1" == "--help" || "$1" == "--version" ]]; then
    echo "Running in non-privileged mode for: $@"
    exec alma "$@"
elif [[ "$1" == "alma" && ("$2" == "--help" || "$2" == "--version") ]]; then
    echo "Running in non-privileged mode for: $@"
    # Skip the first 'alma' argument and pass the rest
    shift
    exec alma "$@"
fi

echo "WARNING: This container runs with privileged access!"

if [ ! -w /dev ]; then
    echo "ERROR: Container must be run with --privileged flag"
    exit 1
fi

if ! lsmod | grep -q loop; then
    modprobe loop 2>/dev/null || echo "WARNING: Could not load loop module"
fi

mkdir -p /output /work

echo "All checks passed. Running: alma $@"
# Check if first argument is 'alma' and skip it to avoid duplication
if [[ "$1" == "alma" ]]; then
    shift
fi
exec alma "$@"
