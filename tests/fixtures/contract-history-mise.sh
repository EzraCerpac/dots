#!/bin/sh
set -eu

while [ "$#" -gt 0 ]; do
    case "$1" in
        -C|--cd) cd "$2"; shift 2 ;;
        -E|--env) export MISE_ENV="$2"; shift 2 ;;
        -q|--quiet) shift ;;
        *) break ;;
    esac
done
printf '%s\n' "$*" >> "$CONTRACT_HISTORY_LOG"

real_mise() {
    PATH="$CONTRACT_HISTORY_REAL_PATH" exec mise "$@"
}

case "$*" in
    'bootstrap dotfiles paths --json')
        if [ "${CONTRACT_HISTORY_PATHS+x}" ]; then
            printf '%s\n' "$CONTRACT_HISTORY_PATHS"
        else
            real_mise "$@"
        fi
        ;;
    'bootstrap dotfiles status --json') printf '%s\n' "$CONTRACT_HISTORY_STATUS" ;;
    'bootstrap dotfiles save'|'bootstrap dotfiles sync') ;;
    'bootstrap packages status --json'|ls|ls\ *) printf '{}\n' ;;
    'settings get history.sync') printf 'manual\n' ;;
    'config get --file '*|'config set '*|'config ls --json'|'--no-config config '*) real_mise "$@" ;;
    'config ls'|install|upgrade\ *|'self-update --yes') ;;
    exec\ *) shift; [ "$1" != "--" ] || shift; exec "$@" ;;
    which\ *) exit 1 ;;
    --version) printf '2026.11.9\n' ;;
    *) printf 'unexpected fixture mise arguments: %s\n' "$*" >&2; exit 1 ;;
esac
