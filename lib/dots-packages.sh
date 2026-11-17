#!/usr/bin/env bash

# mise still reports the merged, host-filtered declarations when no manager
# is enabled. Inventory must never probe package state, including MAS.
setup_package_inventory() {
    MISE_QUIET=1 MISE_SYSTEM_PACKAGES_MANAGERS= setup_mise bootstrap packages status --json
}

# Combine the inventory with mise's effective manager settings. Bare keys
# have no resolved manager in the inventory, so keep their native candidates.
setup_package_scope() {
    local inventory="$1" settings="$2"
    setup_mise exec -- jq -nr --argjson inventory "$inventory" --argjson effective "$settings" '
        $effective.system_packages as $settings
        | ($inventory | keys - ["unprefixed"]) +
          (if $inventory.unprefixed then
              $settings.prefer // ["apt", "dnf", "pacman", "zypper", "apk", "brew", "winget", "scoop", "choco"]
           else [] end)
        | unique
        | map(select(. as $manager | $settings.managers == null or ($settings.managers | index($manager))))
        | join(",")
    '
}

setup_package_scope_without_mas() {
    local scope=",$1,"
    scope="${scope/,mas,/,}"
    scope="${scope#,}"
    printf '%s' "${scope%,}"
}

# status and plan have no --manager selector. Limit their package queries
# with the same native allowlist that the package status command honors.
setup_package_inspect() {
    local inventory settings scope=""
    inventory="$(setup_package_inventory)" || return $?
    if [[ "$inventory" != '{}' ]]; then
        settings="$(setup_mise settings ls --all --json system_packages)" || return $?
        scope="$(setup_package_scope "$inventory" "$settings")" || return $?
    fi
    MISE_SYSTEM_PACKAGES_MANAGERS="$(setup_package_scope_without_mas "$scope")" setup_mise bootstrap "$@"
}
