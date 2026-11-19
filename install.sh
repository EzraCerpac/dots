#!/bin/sh
{
set -eu
minimum_mise=2026.11.8
original_path=$PATH

fail() { printf 'dots installer: %s\n' "$*" >&2; exit 1; }

validate_source() {
    host='[a-zA-Z0-9][a-zA-Z0-9.-]*'
    login='[a-zA-Z0-9_][a-zA-Z0-9._-]*'
    path='[^/:@][^:@]*'
    case "$source" in
        *[!a-zA-Z0-9._~+/@:-]*) ;;
        *) printf '%s\n' "$source" | LC_ALL=C grep -Eq \
            "^([a-zA-Z0-9][a-zA-Z0-9-]*/[a-zA-Z0-9_.-]+|https://$host(:[0-9]+)?/$path|ssh://($login@)?$host(:[0-9]+)?/$path|($login@)?$host:/?$path)$" && return ;;
    esac
    fail '--from requires OWNER/NAME, an HTTPS URL, or an SSH Git address without credentials.'
}

supported_mise() {
    reported=$("$1" --version) || return 1
    printf '%s\n' "$reported" | LC_ALL=C awk -v minimum="$minimum_mise" '{
        if ($1 !~ /^[0-9]+\.[0-9]+\.[0-9]+$/) exit 1
        split($1, found, "."); split(minimum, required, ".")
        for (i=1; i<=3; i++) if (found[i]+0 != required[i]+0) exit(found[i]+0 < required[i]+0)
        exit 0
    }'
}

find_mise() {
    if [ "${DOTS_MISE_BIN+x}" ]; then
        mise=$DOTS_MISE_BIN
        [ -f "$mise" ] && [ -x "$mise" ] || fail 'DOTS_MISE_BIN must point to an executable mise.'
        case "$mise" in /*) ;; *) mise=$PWD/$mise ;; esac
        supported_mise "$mise" || fail "Update the mise at DOTS_MISE_BIN to $minimum_mise or newer, or unset DOTS_MISE_BIN."
        return
    fi
    mise=$HOME/.local/bin/mise
    if [ ! -f "$mise" ] || [ ! -x "$mise" ]; then mise=$(command -v mise || :); fi
    case "$mise" in ''|/*) ;; *) mise=$PWD/$mise ;; esac
    if [ -f "$mise" ] && [ -x "$mise" ]; then
        supported_mise "$mise" && return
        printf 'mise %s or newer is required, so installing standalone mise at ~/.local/bin/mise.\n' "$minimum_mise" >&2
    fi
    mise=$HOME/.local/bin/mise
    installer=$(curl -fsSL https://mise.run)
    printf '%s\n' "$installer" | MISE_INSTALL_PATH="$mise" MISE_INSTALL_HELP=0 sh >&2
    supported_mise "$mise" || fail "the installed mise must be $minimum_mise or newer."
}

clone_root() {
    branch=main
    if git ls-remote --exit-code --heads -- "$repo" refs/heads/dev >/dev/null; then
        branch=dev
    elif [ "$?" -ne 2 ]; then
        exit 1
    fi
    if [ -e "$root" ] || [ -L "$root" ]; then
        [ ! -e "$root/.git" ] && [ ! -L "$root/.git" ] || fail "Move the Git checkout at $root away or drop --from to keep it."
        backup=$root.before-dots-$(date -u +%Y%m%dT%H%M%SZ)
        [ ! -e "$backup" ] && [ ! -L "$backup" ] || fail "backup path already exists at $backup."
        mv -n "$root" "$backup"
        [ ! -e "$root" ] && [ ! -L "$root" ] || fail "could not move the existing configuration to $backup."
        printf 'Moved the existing configuration to %s.\n' "$backup"
    fi
    git clone --branch "$branch" -- "$repo" "$root"
    [ "$(git -C "$root" config --local --get-all remote.origin.url)" = "$repo" ] ||
        fail 'the saved origin does not match --from.'
}

shell_word() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }

retry() {
    printf 'curl -fsSL https://raw.githubusercontent.com/EzraCerpac/dots/main/install.sh | env '
    set -- "DOTS_ROOT=$root" "DOTS_MISE_BIN=$mise" sh -s -- --version "$version"
    if [ "${MISE_YES:-}" = 1 ]; then set -- MISE_YES=1 "$@"; fi
    if [ -n "$archive" ]; then set -- "$@" --archive "$archive"; fi
    if [ -n "$profile" ]; then set -- "$@" --profile "$profile"; fi
    for arg do shell_word "$arg"; printf ' '; done
    case "$source" in
        ssh://*@*) rest=${source#ssh://}; printf "%s 'ssh://''%s''@%s'\n" --from "${rest%%@*}" "${rest#*@}" ;;
        *) printf "%s '%s'\n" --from "$source" ;;
    esac
}

adopt() {
    "$reuse" || clone_root
    cd -P "$root"
    export DOTS_ROOT="$PWD" DOTS_BOOTSTRAP_ADOPT=1 MISE_CONFIG_DIR="$PWD"
    export MISE_GLOBAL_CONFIG_FILE='' MISE_CONFIG_FILE='' MISE_HISTORY_ENABLED=0
    profile=$("$dots" __check-bootstrap-profile "$profile") || {
        status=$?
        if [ "$status" -eq 2 ]; then printf '%s\n' 'Rerun with a declared profile:' >&2; fi
        retry >&2
        exit "$status"
    }
    trap 'test "$?" -eq 0 || retry >&2' 0
    trap 'exit 130' INT
    if ! "$terminal" && [ "${MISE_YES:-}" != 1 ]; then
        printf '%s\n' 'Adoption needs a terminal or MISE_YES=1.' >&2
        exit 2
    fi
    export SETUP_PROFILE="$profile"
    export MISE_TRUSTED_CONFIG_PATHS="${MISE_TRUSTED_CONFIG_PATHS:+$MISE_TRUSTED_CONFIG_PATHS:}$PWD"
    "$mise" bootstrap --skip-dirty --env "$profile"
    "$dots" __check-bootstrap-profile "$profile" >/dev/null
    "$dots" bootstrap persist-selected
}

version=${DOTS_VERSION:-latest} source='' profile='' archive=''
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
            printf '%s\n' 'Usage: sh -s -- [--version VERSION] [--from REPO [--profile PROFILE]] [--archive PATH]' \
                'Version defaults to DOTS_VERSION or latest. Requires Git and curl.' \
                '--archive PATH is for testing only and takes an absolute local .tar.gz path.'
            exit 0 ;;
        *) fail 'unknown option (see --help).' ;;
    esac
done
[ -n "$source" ] || [ -z "$profile" ] || fail '--profile requires --from.'
case "$version" in *[!a-zA-Z0-9._+-]*) fail 'invalid version.' ;; esac
case "$(uname -s):$(uname -m)" in
    Darwin:x86_64|Darwin:amd64|Darwin:arm64|Darwin:aarch64|\
    Linux:x86_64|Linux:amd64|Linux:arm64|Linux:aarch64) ;;
    *) fail 'supported platforms are macOS and Linux on x86_64 or arm64.' ;;
esac
command -v git >/dev/null 2>&1 || fail 'install Git and rerun.'
global=${MISE_CONFIG_DIR:-"${XDG_CONFIG_HOME:-"$HOME/.config"}/mise"}
root=${DOTS_ROOT:-"$global"}
case "$root" in /*) ;; *) root=$PWD/$root ;; esac
reuse=false
if [ -n "$source" ]; then
    while [ "${root%/}" != "$root" ]; do root=${root%/}; done
    case "$root" in ''|*/.|*/..|*:*) fail 'choose a configuration directory without a colon.' ;; esac
    unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_CONFIG GIT_INDEX_FILE
    unset GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES MISE_CD
    case "$source" in *:*) repo=$source ;; *) repo=https://github.com/$source ;; esac
    if [ -e "$root/.git" ] || [ -L "$root/.git" ]; then
        [ "$(git -C "$root" rev-parse --show-toplevel 2>/dev/null)" = "$(cd -P "$root" && pwd)" ] &&
            [ "$(git -C "$root" config --local --get-all remote.origin.url 2>/dev/null)" = "$repo" ] ||
            fail "Move the Git checkout at $root away or drop --from to keep it."
        reuse=true
    fi
fi
tool=github:EzraCerpac/dots@$version
if [ -n "$archive" ]; then
    case "$archive" in /*.tar.gz) ;; *) fail 'expected an absolute .tar.gz path.' ;; esac
    [ "$version" != latest ] || version=0.0.0
    tool="http:dots[url=file://$archive]@$version"
fi
find_mise
export DOTS_MISE_BIN="$mise" DOTS_ROOT="$root" PATH="${mise%/*}:$PATH"
terminal=false
if ( : </dev/tty ) 2>/dev/null; then exec </dev/tty; terminal=true; fi
if [ -n "$source" ]; then
    "$mise" -q -C "$HOME" install "$tool"
    dots=$("$mise" -C "$HOME" which --tool "$tool" dots)
    export PATH="${dots%/*}:$PATH"
    (adopt)
fi
config=${MISE_GLOBAL_CONFIG_FILE:-"${MISE_CONFIG_FILE:-"$global/config.local.toml"}"}
case "$config" in /*) ;; *) config=$PWD/$config ;; esac
"$mise" -q -C "$HOME" use -g --path "$config" --fuzzy "$tool"
"$mise" -C "$HOME" reshim
data=${MISE_DATA_DIR:-"${XDG_DATA_HOME:-"$HOME/.local/share"}/mise"}
shims=$("$mise" -C "$HOME" settings get shims_dir 2>/dev/null) || shims=
shim=${shims:-"${MISE_SHIMS_DIR:-"$data/shims"}"}/dots
case "$shim" in /*) ;; *) shim=$HOME/$shim ;; esac
bin=$HOME/.local/bin
[ -x "$shim" ] || fail 'the mise dots shim is missing.'
if [ ! -e "$bin/dots" ] && [ ! -L "$bin/dots" ]; then
    mkdir -p "$bin"
    ln -s "$shim" "$bin/dots"
fi
launcher=$bin/dots display=dots
if [ "$launcher" != "$shim" ] && [ "$(readlink "$launcher" 2>/dev/null || :)" != "$shim" ]; then
    display=$(shell_word "$shim")
    trap 'printf "Use \"%s\" to run the installed dots alongside your existing entry.\n" "$shim" >&2' 0
elif [ "$launcher" != "$(PATH=$original_path command -v dots || :)" ]; then
    display=$(shell_word "$launcher")
    case "${SHELL:-}" in
        */fish|fish) trap 'printf "fish_add_path ~/.local/bin\n" >&2' 0 ;;
        *) trap 'printf "export PATH=\"\$HOME/.local/bin:\$PATH\"\n" >&2' 0 ;;
    esac
fi
[ "$root" = "$global" ] || display="env DOTS_ROOT=$(shell_word "$root") $display"

if [ -n "$source" ]; then
    printf 'Next: %s up\n' "$display"
elif "$terminal"; then
    dots=$("$mise" -C "$HOME" which --tool "$tool" dots)
    "$dots" init
    if [ "$display" != dots ]; then printf 'Next: %s up\n' "$display"; fi
else
    printf '%s init\n' "$display"
fi
exit 0
}
