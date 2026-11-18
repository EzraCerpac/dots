#!/bin/sh
set -eu
case "$1" in
    -o) (umask 077; set -C; printf '%s\n' "$CONTRACT_HISTORY_MACHINE_KEY" > "$2") ;;
    -y) cat "$2" ;;
    *) exit 1 ;;
esac
