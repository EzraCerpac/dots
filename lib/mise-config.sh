# File-local TOML edits also run before mise has installed any tools.

mise_config_target() {
    local path="$1" target directory links=0
    while [[ -L "$path" ]]; do
        ((links += 1))
        ((links <= 40)) || return 1
        target="$(readlink "$path")" || return 1
        case "$target" in /*) path="$target" ;; *) path="$(dirname "$path")/$target" ;; esac
    done
    if [[ -e "$path" && ! -f "$path" ]]; then
        printf 'dots: refusing non-regular configuration: %s\n' "$path" >&2
        return 2
    fi
    directory="$(cd -P "$(dirname "$path")" && pwd -P)" || return 1
    printf '%s/%s\n' "$directory" "${path##*/}"
}

mise_config_copy() {
    if [[ -f "$1" ]]; then cp "$1" "$2" && chmod u+w "$2"; else : > "$2"; fi
}

mise_config_mode() {
    local target="$1" candidate="$2" mode="$3"
    if [[ -f "$target" ]]; then
        mode="$(stat -c '%a' "$target" 2>/dev/null)" || mode="$(stat -f '%Lp' "$target")" || return 1
    fi
    chmod "$mode" "$candidate"
}

mise_config_install() {
    if [[ -f "$1" ]] && cmp -s "$1" "$2"; then return 0; fi
    mv -f "$2" "$1"
}

mise_config_file() {
    local mise="$1" action="$2" candidate="$3"
    shift 3
    # Trust only the private candidate while mise validates the edit. Do not
    # load or persist trust in any of the caller's configuration.
    MISE_TRUSTED_CONFIG_PATHS="$(dirname "$candidate")" \
        "$mise" --quiet --no-config config "$action" --file "$candidate" "$@"
}

# Keep the original spelling for a semantic no-op. On a real edit the native
# setter may drop decoration from the replaced value. Ask mise which hashes
# start comments so those can stay next to the replacement.
mise_config_keep_source() {
    local mise="$1" before="$2" candidate="$3" patch="$3.diff" probe="$3.probe.toml"
    local prior old new line tail column rendered
    if cmp -s "$before" "$candidate"; then return 0; fi
    prior="$(mise_config_file "$mise" get "$before")" || return $?
    rendered="$(mise_config_file "$mise" get "$candidate")" || return $?
    if [[ "$rendered" == "$prior" ]]; then
        mv -f "$before" "$candidate"
        return $?
    fi
    diff -U0 "$before" "$candidate" > "$patch" || [[ $? -eq 1 ]] || return 1
    : > "$candidate.comments" || return 1
    while read -r old new; do
        line="$(sed -n "${old}p" "$before")" || return $?
        tail="$line"
        while [[ "$tail" == *'#'* ]]; do
            tail="${tail#*#}"
            column="$((${#line} - ${#tail} - 1))"
            awk -v row="$old" -v column="$column" 'NR == row { $0 = substr($0, 1, column) } { print }' "$before" > "$probe" || return $?
            if rendered="$(mise_config_file "$mise" get "$probe" 2>/dev/null)" && [[ "$rendered" == "$prior" ]]; then
                printf '%s\t#%s\n' "$new" "$tail" >> "$candidate.comments" || return $?
                break
            fi
        done
    done < <(awk '
        /^@@ / { old = -$2; new = $3 + 0; hunk = 1; next }
        hunk && /^-/ { if (/#/) print old, new; old++ }
    ' "$patch")
    if [[ -s "$candidate.comments" ]]; then
        awk -F '\t' '
            NR == FNR { comments[$1] = comments[$1] substr($0, index($0, "\t") + 1) "\n"; next }
            { printf "%s%s\n", comments[FNR], $0 }
        ' "$candidate.comments" "$candidate" > "$probe" || return $?
        mv -f "$probe" "$candidate"
    fi
}
