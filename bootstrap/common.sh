#!/usr/bin/env bash

source "$(dirname "${BASH_SOURCE[0]}")/../lib/setup-common.sh"
source "$(dirname "${BASH_SOURCE[0]}")/../lib/mise-config.sh"

bootstrap_error() {
    printf 'bootstrap: %s\n' "$*" >&2
}

bootstrap_init() {
    if [[ -z "${HOME:-}" ]]; then
        bootstrap_error 'HOME is not set'
        return 2
    fi

    # dots internal selected the configuration; never guess it here.
    BOOTSTRAP_CONFIG_DIR="${DOTS_ROOT:?run through dots internal}"
    if [[ ! -d "$BOOTSTRAP_CONFIG_DIR" ]]; then
        bootstrap_error "configuration does not exist: $BOOTSTRAP_CONFIG_DIR"
        return 2
    fi
    BOOTSTRAP_CONFIG_DIR="$(cd "$BOOTSTRAP_CONFIG_DIR" && pwd -P)" || return 1
    BOOTSTRAP_HOME="$(cd "$HOME" && pwd -P)" || return 1
    BOOTSTRAP_MISE_ENV="${SETUP_PROFILE},host-${SETUP_MACHINE_ID}"

    BOOTSTRAP_MISE_BIN="$(resolve_mise_bin)" || {
        local resolve_status=$?
        bootstrap_error 'could not select a mise executable; use SETUP_MISE_BIN or install mise in ~/.local/bin or PATH'
        return "$resolve_status"
    }

    export MISE_CONFIG_DIR="$BOOTSTRAP_CONFIG_DIR"
}

# Load [vars] dots_profiles from the configuration being bootstrapped.
bootstrap_load_profiles() {
    SETUP_ROOT="$BOOTSTRAP_CONFIG_DIR"
    setup_dots_profiles
}

bootstrap_validate_profile() {
    bootstrap_load_profiles || return $?
    if ! setup_is_profile "${SETUP_PROFILE:-}"; then
        bootstrap_error "profile must be one of: ${SETUP_DOTS_PROFILES// /, }"
        return 2
    fi

    if [[ -z "${SETUP_MACHINE_ID:-}" || ! "$SETUP_MACHINE_ID" =~ ^[a-z0-9][a-z0-9-]*$ ]]; then
        bootstrap_error 'machine id must contain lowercase letters, digits, and hyphens'
        return 2
    fi
}

bootstrap_history_branch() {
    if [[ -z "${SETUP_MACHINE_ID:-}" || ! "$SETUP_MACHINE_ID" =~ ^[a-z0-9][a-z0-9-]*$ ]]; then
        bootstrap_error 'machine id is required to derive the history branch'
        return 2
    fi
    printf 'host-%s\n' "$SETUP_MACHINE_ID"
}

bootstrap_mise() {
    "$BOOTSTRAP_MISE_BIN" --quiet --cd "$BOOTSTRAP_CONFIG_DIR" --env "$BOOTSTRAP_MISE_ENV" "$@"
}

bootstrap_miserc_env_values() {
    local path="${1:-$BOOTSTRAP_CONFIG_DIR/miserc.toml}" raw
    local array='^\[("[A-Za-z0-9_.-]+"(, "[A-Za-z0-9_.-]+")*)?\]$'
    [[ -e "$path" || -L "$path" ]] || return 0
    if [[ -L "$path" || ! -f "$path" ]]; then
        bootstrap_error "refusing non-regular mise early config: $path"
        return 2
    fi

    # Ask mise for the root key. An env in a subtable is unrelated to this
    # early selector. A missing root key is fine in a valid TOML document.
    mise_config_file "$BOOTSTRAP_MISE_BIN" get "$path" >/dev/null || return $?
    raw="$(mise_config_file "$BOOTSTRAP_MISE_BIN" get "$path" env 2>/dev/null)" || return 0
    if [[ ! "$raw" =~ $array ]]; then
        bootstrap_error 'miserc env must be an array of selector names'
        return 2
    fi
    printf '%s\n' "$raw" | tr -d '[]" ' | tr ',' '\n'
}

# mise config set treats env arrays as arrays of variable tables, so it cannot
# edit an early selector. Use mise's reader to confirm the extent and scope of
# the assignment before splicing it. Brackets in comments or strings and keys
# in other tables are only possible locations, never proof of a match.
bootstrap_miserc_range() {
    local path="$1" staging="$2" start end
    while read -r start end; do
        sed -n "${start},${end}p" "$path" > "$staging/selector.toml" || return $?
        mise_config_file "$BOOTSTRAP_MISE_BIN" get "$staging/selector.toml" env >/dev/null 2>&1 || continue
        head -n "$end" "$path" > "$staging/prefix.toml" || return $?
        mise_config_file "$BOOTSTRAP_MISE_BIN" get "$staging/prefix.toml" env >/dev/null 2>&1 || continue
        printf '%s %s\n' "$start" "$end"
        return 0
    done < <(awk '
        /^[[:blank:]]*(env|"([^"\\]|\\.)*"|\047[^\047]*\047)[[:blank:]]*=[[:blank:]]*\[/ { start = NR }
        start && /]/ { print start, NR }
    ' "$path")
    bootstrap_error 'could not locate the top-level miserc env array'
    return 2
}

bootstrap_stage_miserc() {
    local path="$1" candidate="$2" values selector previous host_count=0 range start end line key
    local -a selectors=("$SETUP_PROFILE" "host-$SETUP_MACHINE_ID")
    values="$(bootstrap_miserc_env_values "$path")" || return $?
    while IFS= read -r selector; do
        [[ -n "$selector" ]] || continue
        if setup_is_profile "$selector"; then continue; fi
        if [[ "$selector" =~ ^host-[a-z0-9][a-z0-9-]*$ ]]; then
            ((host_count += 1))
            if ((host_count > 1)); then
                bootstrap_error 'miserc has multiple host identities; resolve them before changing the profile'
                return 2
            fi
            continue
        fi
        for previous in "${selectors[@]}"; do
            [[ "$previous" != "$selector" ]] || continue 2
        done
        selectors+=("$selector")
    done <<< "$values"

    if [[ -f "$path" ]] && mise_config_file "$BOOTSTRAP_MISE_BIN" get "$path" env >/dev/null 2>&1; then
        range="$(bootstrap_miserc_range "$path" "$(dirname "$candidate")")" || return $?
        read -r start end <<< "$range"
        key="$(sed -n "${start}p" "$path" | cut -d= -f1)"
        if [[ "$values" == "$(printf '%s\n' "${selectors[@]}")" ]]; then
            mise_config_copy "$path" "$candidate"
            return $?
        fi
    else
        start=1 end=0 key='env '
    fi

    printf -v line ', "%s"' "${selectors[@]}"
    {
        if ((start > 1)); then head -n "$((start - 1))" "$path" || return $?; fi
        # Selector names contain no #, so keep any comments on the old array.
        if ((start <= end)); then
            sed -n "${start},${end}s/^[^#]*\(#.*\)$/\1/p" "$path" || return $?
        fi
        printf '%s= [%s]\n' "$key" "${line:2}"
        tail -n "+$((end + 1))" "$path" || return $?
    } > "$candidate"
}

bootstrap_local_value() {
    local candidate="$1" key="$2" value="$3" type="${4:-string}"
    local before="$candidate.before.toml"
    cp "$candidate" "$before" || return $?
    mise_config_file "$BOOTSTRAP_MISE_BIN" set "$candidate" --type "$type" -- "$key" "$value" || return $?
    mise_config_keep_source "$BOOTSTRAP_MISE_BIN" "$before" "$candidate"
}

bootstrap_append_local() {
    local key="$1" value="$2" type="$3"
    local path="$BOOTSTRAP_CONFIG_DIR/config.local.toml"
    bootstrap_mise config set --append --type "$type" --file "$path" "$key" "$value"
}

bootstrap_stage_local() {
    local candidate="$1" identity_label="$2" history_branch
    history_branch="$(bootstrap_history_branch)" || return $?
    bootstrap_local_value "$candidate" 'env."SETUP_PROFILE"' "$SETUP_PROFILE" || return $?
    bootstrap_local_value "$candidate" 'env."SETUP_MACHINE_ID"' "$SETUP_MACHINE_ID" || return $?
    bootstrap_local_value "$candidate" 'settings."history".sync' manual || return $?

    if [[ -n "$identity_label" ]]; then
        bootstrap_local_value "$candidate" 'env."SETUP_AGE_IDENTITY"' "$identity_label" || return $?
        bootstrap_local_value "$candidate" 'settings."age".identity_files' "$identity_label" list || return $?
    fi

    if [[ -n "${SETUP_HISTORY_ORIGIN:-}" ]]; then
        bootstrap_local_value "$candidate" 'env."SETUP_HISTORY_ORIGIN"' "$SETUP_HISTORY_ORIGIN" || return $?
        # A branch-only origin table is invalid; persist the URL first.
        bootstrap_local_value "$candidate" 'history."origin".url' "$SETUP_HISTORY_ORIGIN" || return $?
        bootstrap_local_value "$candidate" 'history."origin".branch' "$history_branch" || return $?
    fi
}

bootstrap_persist_profile() (
    local identity_label="${1:-}" selector="${2:-update}" local_path local_candidate
    local miserc_path miserc_candidate values
    local_stage='' miserc_stage=''
    bootstrap_init || return $?
    bootstrap_validate_profile || return $?
    miserc_path="$BOOTSTRAP_CONFIG_DIR/miserc.toml"
    local_path="$(mise_config_target "$BOOTSTRAP_CONFIG_DIR/config.local.toml")" || return $?
    if [[ "$local_path" == "$miserc_path" ]]; then
        bootstrap_error 'config.local.toml and miserc.toml must be separate files'
        return 2
    fi
    trap '[[ -z "$local_stage" ]] || rm -rf "$local_stage"; [[ -z "$miserc_stage" ]] || rm -rf "$miserc_stage"' EXIT

    if [[ "$selector" == update ]]; then
        miserc_stage="$(mktemp -d "$BOOTSTRAP_CONFIG_DIR/.dots-profile.XXXXXX")" || return 1
        miserc_candidate="$miserc_stage/miserc.toml"
        if [[ -e "$miserc_path" || -L "$miserc_path" ]]; then
            bootstrap_stage_miserc "$miserc_path" "$miserc_candidate" || return $?
        else
            printf 'env = ["%s", "host-%s"]\n' "$SETUP_PROFILE" "$SETUP_MACHINE_ID" > "$miserc_candidate" || return $?
        fi
        values="$(bootstrap_miserc_env_values "$miserc_candidate")" || return $?
        if [[ "$values" != "$SETUP_PROFILE"$'\n'"host-$SETUP_MACHINE_ID" && "$values" != "$SETUP_PROFILE"$'\n'"host-$SETUP_MACHINE_ID"$'\n'* ]]; then
            bootstrap_error 'staged miserc does not select the requested profile and host'
            return 2
        fi
        mise_config_mode "$miserc_path" "$miserc_candidate" 600 || return $?
    fi

    local_stage="$(mktemp -d "$(dirname "$local_path")/.dots-profile.XXXXXX")" || return 1
    local_candidate="$local_stage/config.local.toml"
    mise_config_copy "$local_path" "$local_candidate" || return $?
    bootstrap_stage_local "$local_candidate" "$identity_label" || return $?
    mise_config_file "$BOOTSTRAP_MISE_BIN" get "$local_candidate" >/dev/null || return $?
    mise_config_mode "$local_path" "$local_candidate" 600 || return $?

    # Both TOML candidates have been checked before either live file changes.
    mise_config_install "$local_path" "$local_candidate" || return $?
    if [[ "$selector" == update ]]; then
        mise_config_install "$miserc_path" "$miserc_candidate" || return $?
    fi
)
