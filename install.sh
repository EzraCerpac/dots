#!/bin/sh
set -eu

fail() {
    printf 'dots installer: %s\n' "$*" >&2
    exit 1
}

# Print one shell word without evaluating any part of it.
quote() {
    word=$1
    while [ "${word#*\'}" != "$word" ]; do
        printf "'%s'\\\\'" "${word%%\'*}"
        word=${word#*\'}
    done
    printf "'%s'" "$word"
}

install_launcher() {
    user_bin=$HOME/.local/bin
    launcher=$user_bin/dots
    mise_data=${MISE_DATA_DIR:-"${XDG_DATA_HOME:-"$HOME/.local/share"}/mise"}
    shim=${MISE_SHIMS_DIR:-"$mise_data/shims"}/dots
    if [ ! -e "$launcher" ] && [ ! -L "$launcher" ]; then
        case "$old_dots" in
            ''|"$shim") mkdir -p "$user_bin"; ln -s "$dots" "$user_bin" ;;
            *) printf 'Kept existing dots command at %s.\n' "$old_dots"; return ;;
        esac
    fi
    if [ "$launcher" = "$shim" ] || [ ! -f "$launcher" ] || [ ! -x "$launcher" ]; then
        printf 'Kept existing path %s. The installed executable is at %s.\n' "$launcher" "$dots"
    elif [ "$(command -v dots || :)" != "$launcher" ] || ! command -v mise >/dev/null 2>&1; then
        printf '%s\n' 'Put the dots launcher first on PATH. For sh-compatible shells, run this and add it to your shell startup file:'
        printf '  export PATH='
        quote "$user_bin"
        if [ "${mise%/*}" != "$user_bin" ]; then
            printf ':'
            quote "${mise%/*}"
        fi
        printf ':"%s"\n' "\$PATH"
    fi
}

main() {
    version=${DOTS_VERSION:-latest}
    source=
    profile=
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --version|--from|--profile)
                case "${2-}" in
                    ''|-*) fail "$1 needs a value." ;;
                esac
                case "$1" in
                    --version) version=$2 ;;
                    --from) source=$2 ;;
                    --profile) profile=$2 ;;
                esac
                shift 2
                ;;
            -h|--help)
                printf '%s\n' 'Usage: sh -s -- [--version VERSION] [--from OWNER/NAME_OR_GIT_URL [--profile PROFILE]]'
                printf '%s\n' 'Version defaults to DOTS_VERSION or latest. Requires Git and curl.'
                return
                ;;
            *) fail "unknown argument $1 (see --help)." ;;
        esac
    done
    [ -n "$source" ] || [ -z "$profile" ] || fail "--profile requires --from."
    case "$profile" in
        -*|*[!a-z0-9-]*) fail "invalid profile name." ;;
    esac
    case "$version" in
        *[!a-zA-Z0-9._+-]*) fail "invalid version." ;;
    esac

    case "$(uname -s):$(uname -m)" in
        Darwin:x86_64|Darwin:amd64|Darwin:arm64|Darwin:aarch64|\
        Linux:x86_64|Linux:amd64|Linux:arm64|Linux:aarch64) ;;
        *) fail "supported platforms are macOS and Linux on x86_64 or arm64." ;;
    esac
    command -v git >/dev/null 2>&1 || fail "install Git and rerun."

    mise_path=${MISE_INSTALL_PATH:-"$HOME/.local/bin/mise"}
    mise=$(command -v mise || :)
    if [ -z "$mise" ]; then
        mise=$mise_path
        if [ ! -x "$mise" ]; then
            installer=$(curl -fsSL https://mise.run)
            printf '%s\n' "$installer" | MISE_INSTALL_PATH="$mise" sh
        fi
    fi
    case "$mise" in /*) ;; *) mise=$PWD/$mise ;; esac

    old_dots=$(command -v dots || :)
    tool=github:EzraCerpac/dots@$version
    # Testing only: use an absolute path to a local release-layout .tar.gz.
    # The HTTP backend reads it from disk and has no "latest" version list.
    if [ -n "${DOTS_INSTALL_ARCHIVE:-}" ]; then
        case "$DOTS_INSTALL_ARCHIVE" in /*.tar.gz) ;; *) fail "expected an absolute .tar.gz path." ;; esac
        [ "$version" != latest ] || version=0.0.0
        tool="http:dots[url=file://$DOTS_INSTALL_ARCHIVE]@$version"
    fi
    # A global `mise use` would create config.toml before bootstrap could
    # adopt into that directory. Install without writing a tool declaration.
    "$mise" -C "$HOME" install "$tool"
    dots=$("$mise" -C "$HOME" which --tool "$tool" dots)
    install_launcher >&2

    root=${DOTS_ROOT-${MISE_CONFIG_DIR-"${XDG_CONFIG_HOME:-"$HOME/.config"}/mise"}}
    case "$root" in /*) ;; *) root=$PWD/$root ;; esac
    setup_path=${dots%/*}:${mise%/*}
    if [ -n "$source" ]; then
        # dots bootstrap needs an existing config.toml. Adopt through mise
        # and use directory discovery so mise loads profile files as well.
        set -- "MISE_CONFIG_DIR=$root" MISE_GLOBAL_CONFIG_FILE= MISE_CONFIG_FILE= \
            MISE_HISTORY_ENABLED=0 "$mise" -C "$HOME" bootstrap --skip-dirty --adopt "$source"
        if [ -n "$profile" ]; then
            set -- "SETUP_PROFILE=$profile" "$@" --env "$profile"
        fi
    else
        set -- "$dots" init
    fi
    set -- env "DOTS_ROOT=$root" "DOTS_MISE_BIN=$mise" "$@"

    if ( : </dev/tty ) 2>/dev/null; then
        PATH="$setup_path:$PATH" exec "$@" </dev/tty
    fi

    printf '%s\n' 'dots is installed. In a terminal, run:'
    printf 'PATH='
    quote "$setup_path"
    printf ':"%s" ' "\$PATH"
    for arg do
        quote "$arg"
        printf ' '
    done
    printf '\n'
}

main "$@"
