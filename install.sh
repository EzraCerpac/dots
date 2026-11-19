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

# shellcheck disable=SC2016
adopt='
mise=$1 dots=$2 root=$3 source=$4 profile=$5 retry=$6
case "$source" in *:*) ;; *) source=https://github.com/$source ;; esac
unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_CONFIG GIT_INDEX_FILE
unset GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES
if [ ! -e "$root/.git" ]; then
    git clone -- "$source" "$root"
fi
origin=$(git -C "$root" config --local --get-all remote.origin.url 2>/dev/null) || origin=
case "$source" in
    https://github.com/*) source=${source%.git}; origin=${origin%.git} ;;
    *@*:/?*|*@*:~/*|git@github.com:?*)
        repo=${source#*:}
        expected=ssh://${source%%:*}/${repo#/}
        if [ "$origin" = "$expected" ]; then origin=$source; fi ;;
esac
if [ "$origin" != "$source" ]; then
    printf "dots installer: the saved origin does not match --from.\n" >&2
    exit 2
fi
cd -P "$root"
root=$PWD
case "$root" in *:*) printf "dots installer: the checkout path cannot contain a colon.\n" >&2; exit 2 ;; esac
export DOTS_ROOT="$root" MISE_CONFIG_DIR="$root" MISE_GLOBAL_CONFIG_FILE= MISE_CONFIG_FILE= MISE_HISTORY_ENABLED=0
if selected=$("$dots" __check-bootstrap-profile "$profile"); then
    export SETUP_PROFILE="$selected"
else
    status=$?
    if [ -n "$selected" ]; then printf "Rerun with a declared profile:\n%s%s\n" "$retry" "$selected" >&2; fi
    exit "$status"
fi
export MISE_TRUSTED_CONFIG_PATHS=${MISE_TRUSTED_CONFIG_PATHS:+$MISE_TRUSTED_CONFIG_PATHS:}$root
"$mise" bootstrap --skip-dirty --env "$selected"
"$dots" __check-bootstrap-profile "$selected" >/dev/null
"$dots" bootstrap persist-selected
'

# Keep separators so the first headless init continuation fits on one line.
# shellcheck disable=SC2016
activate='
mise=$1 config=$2 tool=$3 shims=$4 when=$5; shift 5;
old_dots=$(command -v dots || :);
if [ "$when" = adopt ]; then "$@"; fi;
"$mise" -C "$HOME" use -g --path "$config" --fuzzy "$tool";
"$mise" -C "$HOME" reshim;
configured=$("$mise" -C "$HOME" settings get shims_dir 2>/dev/null) || configured=;
shims=${configured:-$shims};
case "$shims" in /*) ;; *) shims=$HOME/$shims ;; esac;
shim=$shims/dots;
if [ ! -x "$shim" ]; then printf "dots installer: the mise dots shim is missing.\n" >&2; exit 1; fi;
bin=$HOME/.local/bin;
if [ "$bin/dots" != "$shim" ] && [ ! -e "$bin/dots" ] && [ ! -L "$bin/dots" ]; then
    case "$old_dots" in
        ""|"$shim") mkdir -p "$bin"; ln -s "$shim" "$bin" ;;
    esac;
fi;
mise_hint=;
if ! command -v mise >/dev/null 2>&1 && [ ! -x "$bin/mise" ]; then
    mise_hint=" Add \"${mise%/*}\" to PATH for mise.";
fi;
if [ "$bin/dots" = "$shim" ] || [ "$(readlink "$bin/dots" 2>/dev/null || :)" = "$shim" ]; then
    if [ "$(command -v dots || :)" != "$bin/dots" ] || [ -n "$mise_hint" ]; then
        printf "%s%s\n" "Put \$HOME/.local/bin first on PATH to use the dots launcher." "$mise_hint" >&2;
    fi;
else
    printf "Kept your existing dots entry. Run \"%s\" to use the mise-installed dots.%s\n" "$shim" "$mise_hint" >&2;
fi;
if [ "$when" = defer ]; then exec "$@"; fi
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
    shims=${MISE_SHIMS_DIR:-"$data/shims"}
    tty=false
    if ( : </dev/tty ) 2>/dev/null; then tty=true; fi
    when=now
    if [ -n "$source" ]; then when=adopt
    elif ! "$tty" && ! "$mise" -C "$HOME" which dots >/dev/null 2>&1; then when=defer
    fi
    case "$when" in
        now) sh -ec "$activate" sh "$mise" "$config" "$tool" "$shims" "$when" ;;
        defer) "$mise" -q -C "$HOME" install "$tool" ;;
        adopt) "$mise" -C "$HOME" install "$tool" ;;
    esac
    dots=$("$mise" -C "$HOME" which --tool "$tool" dots)
    setup_path=${dots%/*}:${mise%/*}:$PATH
    if [ -n "$source" ]; then
        if [ -n "$archive" ]; then set -- --archive "$archive"; fi
        retry=$(
            printf 'curl -fsSL https://raw.githubusercontent.com/EzraCerpac/dots/main/install.sh | '
            for arg in env "DOTS_ROOT=$root" "DOTS_MISE_BIN=$mise" sh -s -- --from "$source" --version "$version" "$@" --profile; do
                quote "$arg"; printf ' '
            done
        )
        set -- sh -ec "$adopt" sh "$mise" "$dots" "$root" "$source" "$profile" "$retry"
    else
        set -- "$dots" init
    fi
    set -- env "PATH=$setup_path" "DOTS_ROOT=$root" "DOTS_MISE_BIN=$mise" "$@"
    if [ "$when" != now ]; then
        if [ "$when" = defer ]; then activate=$(printf '%s' "$activate" | tr '\n' ' '); fi
        set -- sh -ec "$activate" sh "$mise" "$config" "$tool" "$shims" "$when" "$@"
    fi

    if "$tty"; then exec "$@" </dev/tty; fi
    printf '%s\n' 'dots is installed. In a terminal, run:'
    for arg do quote "$arg"; printf ' '; done
    printf '\n'
}

main "$@"
}
