#!/bin/sh
# Model only the queries and children these tests use. No real mise is invoked.
if [ "${FORBID_MISE+x}" ]; then
    : > "$FORBID_MISE"
    exit 91
fi
while [ "${1-}" = -C ] || [ "${1-}" = -E ]; do shift 2; done
case "${1-} ${2-} ${3-}" in
    'config get --file')
        case "${5-}" in
            vars.dots_profiles) printf 'laptop server\n' ;;
            vars.dots_source_repo) printf 'fixture/config\n' ;;
            *) exit 1 ;;
        esac
        ;;
    'bootstrap dotfiles apply')
        shift 3
        case " $* " in
            *' --dry-run '*|' --yes ') exit 0 ;;
            ' missing-target '|' -- missing-target ')
                printf 'no such target\n' >&2
                exit 1
                ;;
            *) exit 91 ;;
        esac
        ;;
    'bootstrap plan ')
        if [ "${EXPECT_PLAN_SELECTOR+x}" ]; then
            [ "${MISE_CONFIG_DIR-}" = "$DOTS_ROOT" ] &&
                [ "${MISE_ENV-}" = "$EXPECT_PLAN_SELECTOR" ] || exit 91
        fi
        printf 'fixture plan output: preview editor settings\n'
        ;;
    'bootstrap packages status'|'ls --current --json'|'settings ls --all')
        printf '{}\n'
        ;;
    'bootstrap dotfiles status')
        printf '{"files":[],"edits":[]}\n'
        ;;
    'bootstrap dotfiles paths'|'bootstrap dotfiles --local') printf '{"entries":[],"invalid":[]}\n' ;;
    'bootstrap dotfiles save') exit 0 ;;
    'config ls --json') printf '[{"path":"%s/config.toml"}]\n' "$MISE_CONFIG_DIR" ;;
    '--version  ') printf '2026.11.9\n' ;;
    *)
        if [ "${1-}" = exec ]; then
            shift
            [ "${1-}" != -- ] || shift
            exec "$@"
        fi
        printf 'unexpected fixture mise arguments: %s\n' "$*" >&2
        exit 91
        ;;
esac
