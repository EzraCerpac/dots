#!/bin/sh
# Model only the fixture's empty inventories and local command execution.
# Unexpected calls fail closed instead of falling through to a system mise.
set -eu
if [ -f "$DOTS_ROOT/no-more-mise" ]; then
    : > "$DOTS_ROOT/replay-called-mise"
    exit 97
fi
while [ "$#" -gt 0 ]; do
    case "$1" in
        -C|--cd|-E|--env) shift 2 ;;
        --quiet) shift ;;
        *) break ;;
    esac
done
case "${1-} ${2-} ${3-}" in
    'config get --file')
        [ "${5-}" = vars.dots_profiles ] || exit 1
        printf 'laptop\n'
        ;;
    'config ls '*)
        if [ "${FIXTURE_REJECT_CONFIG:-0}" = 1 ]; then
            printf 'fixture-config-rejected\n' >&2
            exit 1
        fi
        printf '[]\n'
        ;;
    'bootstrap dotfiles paths') printf '{"entries":[]}\n' ;;
    'bootstrap dotfiles save'|'install  '|'upgrade --no-prune ') ;;
    'bootstrap packages status'|'ls '*) printf '{}\n' ;;
    'which jq ') command -v jq ;;
    'which '*) exit 1 ;;
    'exec -- '*)
        shift 2
        # The old updater invokes source sync under "env"; this world has no remote.
        if [ "${1-}" = env ] || [ "${1-}" = /usr/bin/env ]; then
            shift
            case "${1-}" in
                -u) shift 2 ;;
                MISE_AUTO_INSTALL=*|MISE_EXEC_AUTO_INSTALL=*) shift ;;
            esac
        fi
        case "${1-}" in
            node) case "${2-}" in */lib/source-repo.mjs) exit 0 ;; esac ;;
            jq|bash|"$DOTS_ROOT"/dots/hooks/update.d/*|*/dots) exec "$@" ;;
        esac
        printf 'fixture mise refused to execute %s\n' "${1-unknown}" >&2
        exit 97
        ;;
    *) printf 'unexpected fixture mise invocation for %s\n' "${1-unknown}" >&2; exit 97 ;;
esac
