package completion

import (
	"crypto/sha256"
	"encoding/json"
	"fmt"
	"os"
	"path"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
)

var applicationsDir = "/Applications"

var githubRepo = regexp.MustCompile(`^https://github\.com/([A-Za-z0-9][A-Za-z0-9-]*)/([A-Za-z0-9_.-]+?)(?:\.git)?/?$`)

func canonicalTarget(key string) string {
	switch {
	case strings.HasPrefix(key, "github:"), strings.HasPrefix(key, "https://"):
		url := key
		if repo, ok := strings.CutPrefix(key, "github:"); ok {
			url = "https://github.com/" + repo
		}
		match := githubRepo.FindStringSubmatch(url)
		if match == nil || match[2] == "." || match[2] == ".." {
			return ""
		}
		return "github:" + strings.ToLower(match[1]+"/"+match[2])
	case strings.HasPrefix(key, "brew:homebrew/core/"):
		return "brew:" + strings.TrimPrefix(key, "brew:homebrew/core/")
	case strings.HasPrefix(key, "brew-cask:homebrew/cask/"):
		return "brew-cask:" + strings.TrimPrefix(key, "brew-cask:homebrew/cask/")
	}
	return key
}

type target struct {
	tool, app bool
	aliases   map[string]bool
}

func (t *target) description(id string) string {
	manager, _, _ := strings.Cut(id, ":")
	switch {
	case t.app:
		return "Managed GitHub app"
	case t.tool:
		return "Declared mise tool"
	default:
		return manager + " package"
	}
}

func (t *target) addAliases(key string) {
	tail := key
	if _, suffix, ok := strings.Cut(key, ":"); ok {
		tail = suffix
	}
	t.aliases[strings.ToLower(tail)] = true
	t.aliases[strings.ToLower(tail[strings.LastIndexByte(tail, '/')+1:])] = true
}

func (t *target) indexAliases(index map[string][]string, id string) {
	for alias := range t.aliases {
		if !slices.Contains(index[alias], id) {
			index[alias] = append(index[alias], id)
		}
	}
}

type targets map[string]*target

func (all targets) ensure(id string) *target {
	if all[id] == nil {
		all[id] = &target{aliases: make(map[string]bool)}
	}
	return all[id]
}

func (all targets) readMarkers(scoped bool, index map[string][]string) {
	state := os.Getenv("XDG_STATE_HOME")
	if state == "" {
		state = filepath.Join(os.Getenv("HOME"), ".local", "state")
	}
	dir := filepath.Join(state, "dots", "github-apps")
	files, _ := os.ReadDir(dir)
	for _, file := range files {
		if !file.Type().IsRegular() || !strings.HasSuffix(file.Name(), ".json") {
			continue
		}
		var owner map[string]string
		data := readRegular(filepath.Join(dir, file.Name()), 16<<10)
		if json.Unmarshal(data, &owner) != nil || len(owner) != 3 ||
			owner["repo"] == "" || owner["path"] == "" || owner["bundle_id"] == "" {
			continue
		}
		id := canonicalTarget("github:" + owner["repo"])
		app := owner["path"]
		name := filepath.Base(app)
		if id == "" || (scoped && all[id] == nil) || !filepath.IsAbs(app) || !strings.HasSuffix(name, ".app") {
			continue
		}
		// Removal indexes schema-valid markers before validating their on-disk ownership.
		// Their aliases can be ambiguous even when the marker cannot be offered.
		record := target{aliases: map[string]bool{strings.ToLower(id): true, strings.ToLower(strings.TrimSuffix(name, ".app")): true}}
		record.addAliases(id)
		record.indexAliases(index, id)
		if app != filepath.Join(applicationsDir, name) || name == ".app" || name == "..app" ||
			!safeText(app) || !safeText(owner["bundle_id"]) ||
			file.Name() != fmt.Sprintf("%x.json", sha256.Sum256([]byte(name))) {
			continue
		}
		info, err := os.Lstat(app)
		if err == nil && !info.IsDir() {
			continue
		}
		parent, err := os.Lstat(applicationsDir)
		if err == nil && !parent.IsDir() {
			continue
		}
		// The marker is the ownership record, even after --keep-installed
		// drops the declaration or the bundle is no longer on disk.
		t := all.ensure(id)
		t.app = true
		for alias := range record.aliases {
			t.aliases[alias] = true
		}
	}
}

func removalTargets(root, prefix string, used map[string]bool, values map[string]string, args []string) candidates {
	all := targets{}
	for _, file := range selectedConfigs(root, used, values) {
		doc := readConfig(file)
		for key := range subtable(doc, "tools") {
			id := canonicalTarget(key)
			if validTarget(id) && validTarget(key) {
				t := all.ensure(id)
				t.tool = true
				t.addAliases(key)
			}
		}
		for key := range subtable(subtable(doc, "bootstrap"), "packages") {
			manager, name, _ := strings.Cut(key, ":")
			id := canonicalTarget(key)
			if name != "" && slices.Contains(packageManagers, manager) && validTarget(id) && validTarget(key) {
				t := all.ensure(id)
				t.addAliases(key)
			}
		}
	}
	index := make(map[string][]string)
	if !used["scope"] || len(all) > 0 {
		all.readMarkers(used["scope"], index)
	}

	for id, t := range all {
		t.aliases[strings.ToLower(id)] = true
		t.indexAliases(index, id)
	}
	resolve := func(word string) string {
		id := canonicalTarget(word)
		if bare, ok := strings.CutPrefix(id, "mise:"); ok && !strings.Contains(bare, ":") {
			if t := all[bare]; t != nil && t.tool {
				return bare
			}
			return ""
		}
		if strings.Contains(id, ":") && all[id] != nil {
			return id
		}
		if ids := index[strings.ToLower(id)]; len(ids) == 1 {
			return ids[0]
		}
		return ""
	}

	taken := make(map[string]bool)
	holds := make(map[string]bool)
	for line := range strings.SplitSeq(string(readRegular(filepath.Join(root, "dots", "exceptions.tsv"), 1<<20)), "\n") {
		fields := strings.Split(line, "\t")
		if len(fields) >= 3 && !strings.HasPrefix(line, "#") {
			for flag := range strings.SplitSeq(strings.TrimSpace(fields[2]), ",") {
				if key, ok := strings.CutPrefix(flag, "hold="); ok {
					holds[key] = true
				}
			}
		}
	}
	for _, word := range args {
		taken[resolve(word)] = true
	}
	result := candidates{}
	prefix = strings.ToLower(prefix)
	for id, t := range all {
		manager, tail, _ := strings.Cut(id, ":")
		name := path.Base(tail)
		base, _, _ := strings.Cut(name, "@")
		if taken[id] || holds[manager+":"+name] || holds[manager+":"+base] {
			continue
		}
		matched, ambiguous := false, false
		for alias := range t.aliases {
			if strings.Contains(alias, ":") || !validTarget(alias) || !strings.HasPrefix(alias, prefix) {
				continue
			}
			if resolve(alias) == id {
				result[alias] = t.description(id)
				matched = true
			} else {
				ambiguous = true
			}
		}
		qualified := id
		if !strings.Contains(id, ":") {
			qualified = "mise:" + id
		}
		if (ambiguous || (!matched && strings.HasPrefix(strings.ToLower(qualified), prefix))) && resolve(qualified) == id {
			result[qualified] = t.description(id)
		}
	}
	return result
}

func validTarget(name string) bool {
	return name != "" && !strings.HasPrefix(name, "-") && !strings.HasPrefix(name, ":") && safeText(name)
}
