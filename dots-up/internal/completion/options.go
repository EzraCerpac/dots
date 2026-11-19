package completion

import "strings"

type option struct {
	name, description, group string
	value, short             string
	inline, repeat           bool
}

var scopeOptions = []option{
	{name: "--base", description: "Select the shared setup configuration", group: "scope"},
	{name: "--profile", description: "Select a profile configuration", group: "scope", value: "profile"},
	{name: "--path", description: "Select a configuration file", group: "scope", value: ":files"},
}

var commandOptions = map[string][]option{
	"add": append(append([]option{}, scopeOptions...),
		option{name: "--asset", description: "Select a GitHub release asset or glob", value: "text"}),
	"remove": {
		scopeOptions[0],
		{name: "--profile", description: "Select a profile configuration", group: "scope", value: "profile", inline: true},
		{name: "--path", description: "Select a configuration file", group: "scope", value: ":files", inline: true},
		{name: "--keep-installed", description: "Remove declarations and retain installed software"},
		{name: "--dry-run", description: "Preview what would be removed"},
	},
	"up": {
		{name: "--last", description: "Open the latest saved update report", group: "update"},
		{name: "--mas", description: "Include Mac App Store updates", group: "update"},
	},
	"sync":    {{name: "--last", description: "Inspect the previous sync"}},
	"restore": {{name: "--initialize-history", description: "Initialize settings history"}},
	"atuin-login": {
		{name: "--browser", description: "Log in through Atuin Hub"},
		{name: "--identity", description: "Read the age identity from a file", value: ":files", inline: true},
	},
	"atuin-bundle": {{name: "--recipient", description: "Encrypt for an age recipient", value: "text", repeat: true}},
	"remote": {
		{name: "--profile", description: "Select the remote machine profile", value: "profile"},
		{name: "--dry-run", description: "Preview the remote bootstrap"},
		{name: "--yes", description: "Skip confirmation"},
		{name: "--identity-file", description: "Use this SSH identity file", value: ":files"},
		{name: "--port", description: "Use this SSH port", value: "text"},
		{name: "--connect-timeout", description: "Set the SSH connection timeout", value: "text"},
		{name: "--github-relay-repo", description: "Select the GitHub relay repository", value: "text"},
		{name: "--github-relay-read-only", description: "Use a read-only GitHub relay"},
	},
	"bootstrap": {
		{name: "--bundle", description: "Read private inputs from an encrypted bundle", value: ":files"},
		{name: "--identity", description: "Read the age identity from a file", value: ":files"},
		{name: "--restore", description: "Restore an existing machine identity", value: "text"},
		{name: "--from", description: "Bootstrap from a setup repository", value: "text", inline: true},
		{name: "--adopt", description: "Adopt a setup or settings history repository", value: "text", inline: true},
		{name: "--from-dir", description: "Use this checkout directory", value: ":dirs", inline: true},
		{name: "--config-dir", description: "Use this configuration directory in the checkout", value: "text", inline: true},
		{name: "--replace-history", description: "Replace unrelated settings history"},
		{name: "--take-remote-all", description: "Adopt all remote settings"},
		{name: "--dry-run", short: "-n", description: "Preview bootstrap actions"},
		{name: "--yes", short: "-y", description: "Skip confirmation"},
		{name: "--skip-dirty", description: "Skip repositories with local changes"},
		{name: "--force-dotfiles", description: "Overwrite conflicting files"},
		{name: "--prune-dotfiles", description: "Remove stale dotfiles deployed by groups"},
		{name: "--only", description: "Run only these bootstrap parts", group: "parts", value: "part", inline: true, repeat: true},
		{name: "--skip", description: "Skip these bootstrap parts", group: "parts", value: "part", inline: true, repeat: true},
		{name: "--prompt-secrets", description: "Prompt for missing secrets"},
		{name: "--prompt-vars", description: "Prompt for configuration variables"},
		{name: "--save-env", description: "Save the selected environments"},
		{name: "--env", short: "-E", description: "Select mise environments", value: "profile", inline: true, repeat: true},
		{name: "--update", description: "Refresh metadata and update repositories"},
		{name: "--upgrade", description: "Upgrade installed packages and tools"},
	},
}

var bundleOptions = map[string][]option{
	"create": {
		{name: "--input", description: "Read private inputs from a file", group: "input", value: ":files"},
		{name: "--from-live", description: "Read live private files", group: "input"},
		{name: "--output", description: "Write the encrypted bundle to a file", value: ":files"},
		{name: "--recipient", description: "Encrypt for an age recipient", value: "text", repeat: true},
	},
	"check": {
		{name: "--bundle", description: "Read the encrypted bundle from a file", value: ":files"},
		{name: "--identity", description: "Read the age identity from a file", value: ":files"},
	},
}

func lookupOption(options []option, name string) option {
	for _, flag := range options {
		if flag.name == name || (flag.short != "" && flag.short == name) {
			return flag
		}
	}
	return option{}
}

func markUsed(used map[string]bool, flag option) {
	used[flag.name] = true
	if flag.group != "" {
		used[flag.group] = true
	}
}

func available(used map[string]bool, flag option) bool {
	if (flag.name == "--only" && used["--restore"]) || (flag.name == "--restore" && used["--only"]) {
		return false
	}
	return (flag.repeat || !used[flag.name]) && (flag.group == "" || !used[flag.group] || used[flag.name])
}

func addOptions(result candidates, options []option, used map[string]bool) {
	for _, flag := range options {
		if available(used, flag) {
			result[flag.name] = flag.description
			if flag.short != "" {
				result[flag.short] = flag.description
			}
		}
	}
}

func optionValues(flag option, root, lead, prefix string) (string, candidates) {
	switch flag.value {
	case ":files", ":dirs":
		if lead != "" {
			return flag.value + "=", nil
		}
		return flag.value, nil
	case "profile", "part":
		var names []string
		description := "Setup profile"
		if flag.value == "profile" {
			names = profiles(root)
		} else {
			names = strings.Fields("plugins packages accounts files services firewall compose repos dotfiles mise-shell-activate macos-defaults settings macos-launchd-agents linux-systemd-units user tools task final-hook")
			description = "Bootstrap part"
		}
		if comma := strings.LastIndexByte(prefix, ','); flag.repeat && comma >= 0 {
			lead += prefix[:comma+1]
			prefix = prefix[comma+1:]
		}
		result := candidates{}
		for _, name := range names {
			if strings.HasPrefix(name, prefix) {
				result[lead+name] = description
			}
		}
		return "", result
	}
	return "", nil
}

var packageManagers = strings.Fields("apk apt brew brew-cask dnf mas pacman")

func addPrefixes(positionals []string) candidates {
	result := candidates{}
	for _, manager := range packageManagers {
		result[manager+":"] = "Native package manager"
	}
	for _, backend := range strings.Fields("aqua asdf cargo conda dotnet gem github go npm pipx spm ubi vfox") {
		result[backend+":"] = "Mise tool backend"
	}
	result["https://github.com/"] = "GitHub release"
	for _, word := range positionals {
		delete(result, word)
	}
	return result
}
