#!/bin/sh
{
set -eu

fail() {
    printf 'dots installer: %s\n' "$*" >&2
    exit 1
}

quote() {
    word=$1
    while [ "${word#*\'}" != "$word" ]; do
        printf "'%s'\\\\'" "${word%%\'*}"
        word=${word#*\'}
    done
    printf "'%s'" "$word"
}

validate_source() {
    invalid='--from requires OWNER/NAME, an HTTPS URL, or an SSH Git address without credentials.'
    host='[a-zA-Z0-9][a-zA-Z0-9.-]*'
    login='[a-zA-Z0-9_][a-zA-Z0-9._-]*'
    path='[^/:@][^:@]*'
    case "$source" in http:*|*[!a-zA-Z0-9._~+/@:-]*) fail "$invalid" ;; esac
    case "$source" in
        ssh://*'@'*/*)
            # Keep ordinary SSH logins in scp syntax, out of URI userinfo.
            printf '%s\n' "$source" | LC_ALL=C grep -Eq "^ssh://$login@$host/.+$" ||
                fail "$invalid"
            source=${source#ssh://}
            case "$source" in
                git@github.com/*|*/~/*) source=${source%%/*}:${source#*/} ;;
                *) source=${source%%/*}:/${source#*/} ;;
            esac
            ;;
    esac
    printf '%s\n' "$source" | LC_ALL=C grep -Eq \
        "^([a-zA-Z0-9][a-zA-Z0-9-]*/[a-zA-Z0-9_.-]+|(https|ssh)://$host(:[0-9]+)?/$path|($login@)?$host:/?$path)$" ||
        fail "$invalid"
}

find_mise() {
    for candidate in "${DOTS_MISE_BIN:-}" "$(command -v mise || :)" \
        /opt/homebrew/bin/mise /usr/local/bin/mise "$HOME/.local/bin/mise"; do
        if [ -f "$candidate" ] && [ -x "$candidate" ]; then mise=$candidate; return; fi
    done
    mise=${MISE_INSTALL_PATH:-"$HOME/.local/bin/mise"}
    installer=$(curl -fsSL https://mise.run)
    printf '%s\n' "$installer" | MISE_INSTALL_PATH="$mise" sh
}

# With --from this runs after adoption, which needs an empty destination.
# shellcheck disable=SC2016
activate='
mise=$1 config=$2 tool=$3 shim=$4; shift 4
old_dots=$(command -v dots || :)
if [ "$#" -gt 0 ]; then "$@"; fi
"$mise" -C "$HOME" use -g --path "$config" --fuzzy "$tool"
"$mise" -C "$HOME" reshim
bin=$HOME/.local/bin
if [ "$bin/dots" != "$shim" ] && [ ! -e "$bin/dots" ] && [ ! -L "$bin/dots" ]; then
    case "$old_dots" in
        ""|"$shim") mkdir -p "$bin"; ln -s "$shim" "$bin" ;;
    esac
fi
mise_hint=
if ! command -v mise >/dev/null 2>&1 && [ ! -x "$bin/mise" ]; then
    mise_hint=" Add \"${mise%/*}\" to PATH for mise."
fi
if [ "$bin/dots" = "$shim" ] || [ "$(readlink "$bin/dots" 2>/dev/null || :)" = "$shim" ]; then
    if [ "$(command -v dots || :)" != "$bin/dots" ] || [ -n "$mise_hint" ]; then
        printf "%s%s\n" "Put \$HOME/.local/bin first on PATH to use the dots launcher." "$mise_hint" >&2
    fi
else
    printf "Kept your existing dots entry. Run \"%s\" to use the mise-installed dots.%s\n" "$shim" "$mise_hint" >&2
fi
'

main() {
    version=${DOTS_VERSION:-latest}
    source=
    profile=
    archive=
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --version|--from|--profile|--archive)
                case "${2-}" in ''|-*) fail "$1 needs a value." ;; esac
                case "$1" in
                    --version) version=$2 ;;
                    --from) source=$2; validate_source ;;
                    --profile) profile=$2 ;;
                    --archive) archive=$2 ;;
                esac
                shift 2 ;;
            -h|--help)
                printf '%s\n' 'Usage: sh -s -- [--version VERSION] [--from REPO [--profile PROFILE]] [--archive PATH]'
                printf '%s\n' 'Version defaults to DOTS_VERSION or latest. Requires Git and curl.'
                printf '%s\n' 'For SSH with both a login and a custom port, use an SSH host alias.'
                printf '%s\n' '--archive PATH is for testing only and takes an absolute local .tar.gz path.'
                return ;;
            *) fail 'unknown option (see --help).' ;;
        esac
    done
    [ -n "$source" ] || [ -z "$profile" ] || fail "--profile requires --from."
    case "$profile" in -*|*[!a-z0-9-]*) fail "invalid profile name." ;; esac
    case "$version" in *[!a-zA-Z0-9._+-]*) fail "invalid version." ;; esac
    case "$(uname -s):$(uname -m)" in
        Darwin:x86_64|Darwin:amd64|Darwin:arm64|Darwin:aarch64|\
        Linux:x86_64|Linux:amd64|Linux:arm64|Linux:aarch64) ;;
        *) fail "supported platforms are macOS and Linux on x86_64 or arm64." ;;
    esac
    command -v git >/dev/null 2>&1 || fail "install Git and rerun."

    tool=github:EzraCerpac/dots@$version
    if [ -n "$archive" ]; then
        case "$archive" in /*.tar.gz) ;; *) fail "expected an absolute .tar.gz path." ;; esac
        [ "$version" != latest ] || version=0.0.0
        tool="http:dots[url=file://$archive]@$version"
    fi
    find_mise
    case "$mise" in /*) ;; *) mise=$PWD/$mise ;; esac
    global=${MISE_CONFIG_DIR:-"${XDG_CONFIG_HOME:-"$HOME/.config"}/mise"}
    root=${DOTS_ROOT:-"$global"}
    case "$root" in /*) ;; *) root=$PWD/$root ;; esac
    config=${MISE_GLOBAL_CONFIG_FILE:-"${MISE_CONFIG_FILE:-"$global/config.local.toml"}"}
    case "$config" in /*) ;; *) config=$PWD/$config ;; esac
    data=${MISE_DATA_DIR:-"${XDG_DATA_HOME:-"$HOME/.local/share"}/mise"}
    shim=${MISE_SHIMS_DIR:-"$data/shims"}/dots
    if [ -z "$source" ]; then
        sh -ec "$activate" sh "$mise" "$config" "$tool" "$shim"
    else
        "$mise" -C "$HOME" install "$tool"
    fi
    dots=$("$mise" -C "$HOME" which --tool "$tool" dots)
    setup_path=${dots%/*}:${mise%/*}:$PATH
    if [ -n "$source" ]; then
        set -- "MISE_CONFIG_DIR=$root" MISE_GLOBAL_CONFIG_FILE= MISE_CONFIG_FILE= \
            MISE_HISTORY_ENABLED=0 "$mise" -C "$HOME" bootstrap --skip-dirty --adopt "$source"
        [ -z "$profile" ] || set -- "SETUP_PROFILE=$profile" "$@" --env "$profile"
    else
        set -- "$dots" init
    fi
    set -- env "PATH=$setup_path" "DOTS_ROOT=$root" "DOTS_MISE_BIN=$mise" "$@"
    if [ -n "$source" ]; then
        set -- sh -ec "$activate" sh "$mise" "$config" "$tool" "$shim" "$@"
    fi

    if ( : </dev/tty ) 2>/dev/null; then exec "$@" </dev/tty; fi
    printf '%s\n' 'dots is installed. In a terminal, run:'
    for arg do quote "$arg"; printf ' '; done
    printf '\n'
}

main "$@"
}
