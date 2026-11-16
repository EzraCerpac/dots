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

# Print "AGENT<TAB>LINK" for each agent whose configuration directory exists:
# Claude Code reads ~/.claude/skills; Codex and other Agent Skills clients
# read ~/.agents/skills. An agent's skills directory is created only when the
# agent is in use, so dots never creates ~/.claude for someone without it.
dots_skill_links() {
    local claude="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
    [[ ! -d "$claude" ]] || printf 'claude\t%s/skills/dots\n' "$claude"
    if [[ -d "$HOME/.agents" || -d "${CODEX_HOME:-$HOME/.codex}" ]]; then
        printf 'agents\t%s/.agents/skills/dots\n' "$HOME"
    fi
}

# Whether LINK is a symlink dots made: its target is the skill inside a dots
# tree (one with bin/dots), or such a tree that no longer exists at all.
dots_skill_link_is_ours() {
    local link="$1" target root
    [[ -L "$link" ]] || return 1
    target="$(readlink "$link")"
    [[ "$target" == /*/skills/dots ]] || return 1
    root="${target%/skills/dots}"
    [[ -f "$root/bin/dots" || ! -e "$root" ]]
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
    while IFS=$'\t' read -r agent link; do
        [[ -n "$link" ]] || continue
        [[ "$(readlink "$link" 2>/dev/null)" != "$source" ]] || continue
        if [[ -L "$link" ]] && dots_skill_link_is_ours "$link"; then
            ln -sfn "$source" "$link" 2>/dev/null || true
        elif [[ ! -e "$link" && ! -L "$link" ]]; then
            mkdir -p "${link%/*}" 2>/dev/null && ln -s "$source" "$link" 2>/dev/null || true
        else
            notice="$link"
            if ! grep -qxF "$notice" "$notices" 2>/dev/null; then
                printf 'dots: left %s alone; it is not a dots link (the %s skill stays unlinked)\n' "$link" "$agent" >&2
                { mkdir -p "${notices%/*}" && printf '%s\n' "$notice" >> "$notices"; } 2>/dev/null || true
            fi
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
    while IFS=$'\t' read -r agent link; do
        if dots_skill_link_is_ours "$link"; then state="-> $(readlink "$link")"
        elif [[ -e "$link" || -L "$link" ]]; then state="provided by you, not dots"
        else state="not linked"
        fi
        printf 'Agent skill (%s): %s %s\n' "$agent" "$link" "$state"
    done < <(dots_skill_links)
}
