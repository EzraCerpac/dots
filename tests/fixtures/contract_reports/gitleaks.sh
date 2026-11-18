#!/bin/sh
# Benign fixture input. Report an empty scan and do no scanning or I/O outside the world.
case "${1-}" in
    version|--version) printf '8.30.1\n'; exit 0 ;;
esac
while [ "$#" -gt 0 ]; do
    case "$1" in
        --report-path) printf '[]\n' > "$2"; exit ;;
        --report-path=*) printf '[]\n' > "${1#*=}"; exit ;;
    esac
    shift
done
