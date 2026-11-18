// Package completion implements the read-only, shell-independent completion protocol.
package completion

import (
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/EzraCerpac/dots/dots-up/internal/commands"
	"golang.org/x/sys/unix"
)

type candidates map[string]string

// Run writes the completion response for words after the executable, including
// the current token. Root is selected by the same rule as the normal renderer.
func Run(out io.Writer, words []string, root string) {
	mode, matches := suggest(words, root)
	if mode != "" {
		fmt.Fprintln(out, mode)
		return
	}
	values := make([]string, 0, len(matches))
	for value, description := range matches {
		if value != "" && !strings.HasPrefix(value, ":") && safeText(value) && safeText(description) {
			values = append(values, value)
		}
	}
	slices.Sort(values)
	for _, value := range values {
		fmt.Fprintf(out, "%s\t%s\n", value, matches[value])
	}
}

func safeText(s string) bool {
	return utf8.ValidString(s) && strings.IndexFunc(s, func(r rune) bool {
		return !unicode.IsPrint(r) || unicode.Is(unicode.Zl, r) || unicode.Is(unicode.Zp, r)
	}) == -1
}

var frontendOptions = []option{
	{name: "--plain", description: "Show concise progress", group: "display"},
	{name: "--verbose", description: "Show full output", group: "display"},
	{name: "--no-wait", description: "Close when the command finishes"},
}

var helpOptions = []option{
	{name: "-h", description: "Show help"},
	{name: "--help", description: "Show help"},
}

var extras = candidates{
	"completions": "Print shell completions",
	"menu":        "Open the command menu",
	"help":        "Show commands and usage",
	"init":        "Create a minimal dots configuration",
}

var commandName = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9._-]*$`)

func publicCommands(reports bool) candidates {
	result := candidates{}
	for _, spec := range commands.All() {
		if !reports || (spec.Name != "reports" && spec.Name != "plan") {
			result[spec.Name] = spec.Description
		}
	}
	for _, alias := range []string{"uninstall", "update"} {
		result[alias] = "Alias for " + commands.Canonical(alias)
	}
	return result
}

func topLevel(root, prefix string, used map[string]bool) candidates {
	result := publicCommands(false)
	for name, description := range extras {
		result[name] = description
	}
	addOptions(result, frontendOptions, used)
	if len(used) == 0 {
		addOptions(result, helpOptions, used)
		result["--version"] = "Print the manager version"
	}
	if len(used) == 0 && !strings.HasPrefix(prefix, "-") {
		dir := filepath.Join(root, "dots", "commands")
		entries, _ := os.ReadDir(dir)
		for _, entry := range entries {
			name := entry.Name()
			if _, builtin := result[name]; builtin || name == "internal" || !commandName.MatchString(name) {
				continue
			}
			path := filepath.Join(dir, name)
			if info, err := os.Stat(path); err == nil && info.Mode().IsRegular() && unix.Access(path, unix.X_OK) == nil {
				result[name] = "Configuration command"
			}
		}
	}
	return matching(result, prefix)
}

func matching(all candidates, prefix string) candidates {
	for value := range all {
		if !strings.HasPrefix(value, prefix) {
			delete(all, value)
		}
	}
	return all
}

func suggest(words []string, root string) (string, candidates) {
	prefix := ""
	if len(words) > 0 {
		prefix = words[len(words)-1]
		words = words[:len(words)-1]
	}
	if !safeText(prefix) {
		return "", nil
	}
	shared := make(map[string]bool)
	for len(words) > 0 {
		flag := lookupOption(frontendOptions, words[0])
		if flag.name == "" {
			break
		}
		if flag.group != "" && shared[flag.group] && !shared[flag.name] {
			return "", nil
		}
		markUsed(shared, flag)
		words = words[1:]
	}
	if len(words) == 0 {
		return "", topLevel(root, prefix, shared)
	}

	name := commands.Canonical(words[0])
	if name == "completions" && len(words) == 1 {
		return "", matching(candidates{"bash": "Bash", "fish": "Fish", "zsh": "Zsh"}, prefix)
	}
	if _, ok := commands.Lookup(name); !ok && name != "init" {
		return "", nil
	}
	words = words[1:]
	options := commandOptions[name]
	if name == "bundle" {
		if len(words) == 0 {
			if strings.HasPrefix(prefix, "-") {
				result := candidates{}
				addOptions(result, helpOptions, nil)
				return "", matching(result, prefix)
			}
			return "", matching(candidates{"create": "Encrypt private inputs", "check": "Validate an encrypted bundle"}, prefix)
		}
		options = bundleOptions[words[0]]
		if options == nil {
			return "", nil
		}
		words = words[1:]
	}
	used := make(map[string]bool)
	if name == "up" {
		options = append(slices.Clone(options), frontendOptions...)
		used = shared
	}
	if len(words) == 0 {
		options = append(slices.Clone(options), helpOptions...)
	}

	var positionals []string
	values := make(map[string]string)
	endOptions := false
	var pending option
	for _, word := range words {
		if pending.name != "" {
			if word == "" {
				return "", nil
			}
			values[pending.name] = word
			pending = option{}
			continue
		}
		if !endOptions && (name == "apply" || name == "remove" || name == "bootstrap") && word == "--" {
			endOptions = true
			continue
		}
		if !endOptions && strings.HasPrefix(word, "-") {
			key, value, inline := strings.Cut(word, "=")
			flag := lookupOption(options, key)
			if flag.name == "" || (flag.group != "" && used[flag.group] && !used[flag.name]) ||
				(name == "add" && used[flag.name]) {
				return "", nil
			}
			markUsed(used, flag)
			switch {
			case inline:
				if !flag.inline || value == "" {
					return "", nil
				}
				values[flag.name] = value
			case flag.value != "":
				pending = flag
			}
			continue
		}
		positionals = append(positionals, word)
	}
	if pending.name != "" {
		return optionValues(pending, root, "", prefix)
	}
	if !endOptions && strings.HasPrefix(prefix, "-") {
		if key, value, inline := strings.Cut(prefix, "="); inline {
			flag := lookupOption(options, key)
			if flag.inline && available(used, flag) {
				return optionValues(flag, root, key+"=", value)
			}
			return "", nil
		}
		result := candidates{}
		if name != "remote" || len(positionals) != 0 {
			addOptions(result, options, used)
		} else if len(words) == 0 {
			addOptions(result, helpOptions, used)
		}
		return "", matching(result, prefix)
	}

	result := candidates{}
	switch name {
	case "init":
		if len(positionals) == 0 {
			return ":dirs", nil
		}
	case "reports":
		if len(positionals) == 0 {
			result = publicCommands(true)
		}
	case "test":
		result = candidates{"python": "Python tests", "node": "Node.js tests", "shell": "Shell tests"}
		for _, suite := range positionals {
			delete(result, suite)
		}
	case "add":
		result = addPrefixes(positionals)
	case "remove":
		return "", removalTargets(root, prefix, used, values, positionals)
	}
	return "", matching(result, prefix)
}
