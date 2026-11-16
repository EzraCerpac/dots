# The manager's own installation: its version, how it was installed, and the
# agent skill links that point into it. Sourced by bin/dots and the tasks;
# needs only Bash 3.2, so it also runs before mise has installed anything.

# Print the version of the manager in HOME: the VERSION file a release archive
# carries, else `git describe` for a checkout, else "dev".
dots_manager_version() {
    local home="$1" version=""
    if [[ -f "$home/VERSION" ]]; then
        IFS= read -r version < "$home/VERSION" || true
    elif [[ -e "$home/.git" ]] && command -v git >/dev/null 2>&1; then
        version="$(git -C "$home" describe --tags --always --dirty 2>/dev/null || true)"
        version="${version#v}"
    fi
    printf '%s\n' "${version:-dev}"
}

# Print the manager directory an installed dots executable belongs to: the
# parent of the bin directory its real path is in.
dots_manager_home_of() {
    local source="$1" directory target
    while [[ -L "$source" ]]; do
        directory="$(cd -P "$(dirname "$source")" && pwd)" || return 1
        target="$(readlink "$source")"
        case "$target" in /*) source="$target" ;; *) source="$directory/$target" ;; esac
    done
    directory="$(cd -P "$(dirname "$source")" && pwd)" || return 1
    (cd -P "$directory/.." && pwd)
}

# Print how the manager in HOME was installed: checkout, homebrew, mise or
# archive (an unpacked release anywhere else).
dots_manager_installer() {
    local home="$1"
    if [[ -e "$home/.git" ]]; then printf 'checkout\n'
    elif [[ "$home" == */Cellar/* ]]; then printf 'homebrew\n'
    elif [[ "$home" == */installs/* ]]; then printf 'mise\n'
    else printf 'archive\n'
    fi
}

# Print the path agents should read the skill through. A Homebrew keg is
# replaced on upgrade, so use its stable opt link when that resolves to HOME;
# a mise version directory stays until it is pruned, and every dots run
# relinks to the current one.
dots_skill_source() {
    local home="$1" formula opt
    if [[ "$home" == */Cellar/*/*/libexec ]]; then
        formula="${home#*/Cellar/}"
        opt="${home%%/Cellar/*}/opt/${formula%%/*}/libexec"
        if [[ -d "$opt" && "$(cd "$opt" 2>/dev/null && pwd -P)" == "$home" ]]; then
            printf '%s/skills/dots\n' "$opt"
            return
        fi
    fi
    printf '%s/skills/dots\n' "$home"
}

# Print "AGENT<TAB>LINK<TAB>BASE" for each agent whose configuration directory
# exists: Claude Code reads ~/.claude/skills; Codex and other Agent Skills
# clients read ~/.agents/skills. BASE is the agent directory the link must stay
# inside. An agent's skills directory is created only when the agent is in
# use, so dots never creates ~/.claude for someone without it.
dots_skill_links() {
    local claude="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
    [[ ! -d "$claude" ]] || printf 'claude\t%s/skills/dots\t%s\n' "$claude" "$claude"
    if [[ -d "$HOME/.agents" || -d "${CODEX_HOME:-$HOME/.codex}" ]]; then
        printf 'agents\t%s/.agents/skills/dots\t%s/.agents\n' "$HOME" "$HOME"
    fi
}

# Whether LINK's directory resolves inside BASE, before anything is created.
# BASE itself is taken as given (CLAUDE_CONFIG_DIR may be anywhere, and
# ~/.claude may be a link); a skills directory that is a link leading
# elsewhere is not, so dots never writes outside the agent's directory.
dots_skill_parent_inside() {
    local parent="${1%/*}" base="$2" real_base real_parent
    [[ -e "$base" || -L "$base" ]] || return 0  # mkdir -p creates both as plain directories
    real_base="$(cd -P "$base" 2>/dev/null && pwd)" || return 1
    [[ -e "$parent" || -L "$parent" ]] || return 0
    real_parent="$(cd -P "$parent" 2>/dev/null && pwd)" || return 1
    [[ "$real_parent" == "$real_base" || "$real_parent" == "$real_base"/* ]]
}

# dots records each skill link it creates as LINK<TAB>TARGET, so it replaces
# only a link that still points where dots pointed it: a link the user made or
# repointed, even into another dots tree, is theirs.
dots_skill_record() {
    printf '%s/dots/skill-links\n' "${XDG_STATE_HOME:-$HOME/.local/state}"
}

# Whether LINK is a symlink dots made and nobody has changed since.
dots_skill_link_is_ours() {
    [[ -L "$1" ]] && grep -qxF "$1"$'\t'"$(readlink "$1")" "$(dots_skill_record)" 2>/dev/null
}

# Point LINK at SOURCE and record it; without a record, undo the link so the
# next run retries both instead of treating the link as the user's.
dots_skill_link() {
    local link="$1" source="$2" record kept
    record="$(dots_skill_record)"
    mkdir -p "${link%/*}" "${record%/*}" 2>/dev/null || return 0
    ln -sfn "$source" "$link" 2>/dev/null || return 0
    kept="$(LINK="$link" awk -F '\t' '$1 != ENVIRON["LINK"]' "$record" 2>/dev/null || true)"
    if ! { printf '%s\n' ${kept:+"$kept"} "$link"$'\t'"$source" > "$record.tmp" && mv -f "$record.tmp" "$record"; } 2>/dev/null; then
        rm -f "$link" "$record.tmp" 2>/dev/null || true
    fi
}

# Link the bundled skill into every agent in use, pointing at HOME's skill.
# Silent when nothing changes. A destination the user owns (a directory, a
# file, or another link) is left alone, with one notice per destination;
# DOTS_SKILL_LINKS=0 turns linking off.
dots_skill_refresh() {
    local home="$1" source agent link notices notice
    [[ "${DOTS_SKILL_LINKS:-1}" != 0 && -n "${HOME:-}" ]] || return 0
    [[ -f "$home/skills/dots/SKILL.md" ]] || return 0
    source="$(dots_skill_source "$home")"
    notices="${XDG_STATE_HOME:-$HOME/.local/state}/dots/skill-notices"
    while IFS=$'\t' read -r agent link base; do
        [[ -n "$link" ]] || continue
        [[ "$(readlink "$link" 2>/dev/null)" != "$source" ]] || continue
        if dots_skill_parent_inside "$link" "$base" &&
            { dots_skill_link_is_ours "$link" || [[ ! -e "$link" && ! -L "$link" ]]; }; then
            dots_skill_link "$link" "$source"
            continue
        fi
        notice="$link"
        if ! grep -qxF "$notice" "$notices" 2>/dev/null; then
            printf 'dots: left %s alone; it is not a dots link or its directory leads outside %s (the %s skill stays unlinked)\n' "$link" "$base" "$agent" >&2
            { mkdir -p "${notices%/*}" && printf '%s\n' "$notice" >> "$notices"; } 2>/dev/null || true
        fi
    done < <(dots_skill_links)
}

# Print one status line per agent: where the skill is linked, or why not.
dots_skill_status() {
    local home="$1" agent link state
    if [[ "${DOTS_SKILL_LINKS:-1}" == 0 ]]; then
        printf 'Agent skill: linking turned off (DOTS_SKILL_LINKS=0)\n'
        return 0
    fi
    if [[ -z "$(dots_skill_links)" ]]; then
        printf 'Agent skill: no agent configuration found (~/.claude, ~/.agents or ~/.codex)\n'
        return 0
    fi
    while IFS=$'\t' read -r agent link _; do
        if dots_skill_link_is_ours "$link"; then state="-> $(readlink "$link")"
        elif [[ -e "$link" || -L "$link" ]]; then state="provided by you, not dots"
        else state="not linked"
        fi
        printf 'Agent skill (%s): %s %s\n' "$agent" "$link" "$state"
    done < <(dots_skill_links)
}
