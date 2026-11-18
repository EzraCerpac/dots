package completion

import (
	"bytes"
	"crypto/sha256"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func fixture(t *testing.T, root, name, body string, mode os.FileMode) {
	t.Helper()
	file := filepath.Join(root, name)
	if err := os.MkdirAll(filepath.Dir(file), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(file, []byte(body), mode); err != nil {
		t.Fatal(err)
	}
}

func response(t *testing.T, root string, words ...string) []string {
	t.Helper()
	var out bytes.Buffer
	Run(&out, words, root)
	var result []string
	for line := range strings.SplitSeq(strings.TrimSuffix(out.String(), "\n"), "\n") {
		if line == "" {
			continue
		}
		if line == ":files" || line == ":dirs" {
			result = append(result, line)
			continue
		}
		value, description, ok := strings.Cut(line, "\t")
		if !ok || description == "" || strings.Contains(description, "\t") {
			t.Fatalf("invalid protocol line %q", line)
		}
		result = append(result, value)
	}
	return result
}

func expect(t *testing.T, root string, words, want []string) {
	t.Helper()
	if got := response(t, root, words...); !reflect.DeepEqual(got, want) {
		t.Errorf("complete %q = %q, want %q", words, got, want)
	}
}

func TestCommandGrammar(t *testing.T) {
	root := t.TempDir()
	fixture(t, root, "config.toml", "[vars]\ndots_profiles = '''server laptop'''\n", 0600)
	fixture(t, root, "dots/commands/hello", "", 0700)
	fixture(t, root, "dots/commands/hidden", "", 0600)
	fixture(t, root, "dots/commands/add", "", 0700)
	fixture(t, root, "dots/commands/hello\nworld", "", 0700)
	for _, tc := range []struct{ words, want []string }{
		{[]string{"he"}, []string{"hello", "help"}},
		{[]string{"compl"}, []string{"completions"}},
		{[]string{"completions", ""}, []string{"bash", "fish", "zsh"}},
		{[]string{"--plain", "he"}, []string{"help"}},
		{[]string{"hi"}, nil},
		{[]string{"hello", "--"}, nil},
		{[]string{"up"}, []string{"up", "update"}},
		{[]string{"--plain", "update", "--ver"}, nil},
		{[]string{"up", "--last", "--m"}, nil},
		{[]string{"sync", "--l"}, []string{"--last"}},
		{[]string{"reports", "u"}, []string{"uninstall", "up", "update"}},
		{[]string{"reports", "up", ""}, nil},
		{[]string{"test", "shell", ""}, []string{"node", "python"}},
		{[]string{"add", "--profile", ""}, []string{"laptop", "server"}},
		{[]string{"add", "--profile", "server", "--p"}, nil},
		{[]string{"add", "--path", "dir/a"}, []string{":files"}},
		{[]string{"add", "--path=a"}, nil},
		{[]string{"add", "npm:prettier", "ca"}, []string{"cargo:"}},
		{[]string{"add", "--asset", ""}, nil},
		{[]string{"uninstall", "--profile=la"}, []string{"--profile=laptop"}},
		{[]string{"uninstall", "--path=./set"}, []string{":files"}},
		{[]string{"init", ""}, []string{":dirs"}},
		{[]string{"init", "dir", ""}, nil},
		{[]string{"remote", "host", "--identity-file", ""}, []string{":files"}},
		{[]string{"remote", "host", "--profile", "l"}, []string{"laptop"}},
		{[]string{"bootstrap", "--config-dir", ""}, nil},
		{[]string{"bootstrap", "--restore", "laptop", "--on"}, nil},
		{[]string{"bootstrap", "--only=tools", "--rest"}, nil},
		{[]string{"bootstrap", "--only=tools,dot"}, []string{"--only=tools,dotfiles"}},
		{[]string{"bundle", "ch"}, []string{"check"}},
		{[]string{"bundle", "check", "--identity", ""}, []string{":files"}},
		{[]string{"bundle", "create", "--recipient", "age1", "--rec"}, []string{"--recipient"}},
		{[]string{"--verbose", "--plain", ""}, nil},
		{[]string{"\x1b"}, nil},
	} {
		expect(t, root, tc.words, tc.want)
	}
}

func TestScopedRemove(t *testing.T) {
	root := t.TempDir()
	caller := t.TempDir()
	t.Chdir(caller)
	t.Setenv("XDG_STATE_HOME", t.TempDir())
	t.Setenv("DOTS_MISE_BIN", fixtureProbe(t))
	fixture(t, root, "config.toml", `
[vars]
dots_profiles = "server laptop"
[tools]
fd = "latest"
"npm:@scope/foo" = [{version = "1"}, {version = "2"}]
"bad\nname" = "latest"
[bootstrap.packages]
"brew:homebrew/core/fd" = "latest"
"some-other-manager:unsupported" = "latest"
[Tools]
ignored = "latest"
`, 0600)
	fixture(t, root, "config.server.toml", "[tools]\nserver = 'latest'\n", 0600)
	fixture(t, caller, "chosen.toml", "[tools]\nchosen = { version = 'latest' }\n", 0600)
	fixture(t, caller, "trailing.toml", "[tools]\n'github:Owner/Widget/' = 'latest'\n", 0600)
	expect(t, root, []string{"remove", "--path", "trailing.toml", "wid"}, nil)
	fixture(t, caller, "broken.toml", "[tools]\nbroken = {\n", 0600)
	expect(t, root, []string{"remove", "--base", ""}, []string{"@scope/foo", "brew:fd", "foo", "homebrew/core/fd", "mise:fd"})
	expect(t, root, []string{"remove", "--base", "fd"}, []string{"brew:fd", "mise:fd"})
	expect(t, root, []string{"remove", "--base", "npm:"}, []string{"npm:@scope/foo"})
	expect(t, root, []string{"remove", "--base", "mise:fd", "fd"}, []string{"brew:fd"})
	expect(t, root, []string{"remove", "npm:@scope/foo", "--base", "f"}, []string{"brew:fd", "mise:fd"})
	expect(t, root, []string{"uninstall", "--profile=server", ""}, []string{"server"})
	expect(t, root, []string{"remove", "--profile", "../server", ""}, nil)
	expect(t, root, []string{"remove", "--base", "--profile", "server", ""}, nil)
	expect(t, root, []string{"remove", "--path", "chosen.toml", ""}, []string{"chosen"})
	expect(t, root, []string{"remove", "--path=broken.toml", ""}, nil)
	fixture(t, root, "dots/exceptions.tsv", "server\tmanual\thold=brew:fd\n", 0600)
	expect(t, root, []string{"remove", "--base", "fd"}, []string{"mise:fd"})
	if _, err := os.Stat(os.Getenv("COMPLETION_PROBE_LOG")); !os.IsNotExist(err) {
		t.Fatal("explicit selectors ran the active config probe")
	}
}

func fixtureProbe(t *testing.T) string {
	t.Helper()
	probe := t.TempDir()
	t.Setenv("COMPLETION_PROBE_LOG", filepath.Join(probe, "called"))
	fixture(t, probe, "mise", `#!/bin/sh
printf '%s\n' "$@" > "$COMPLETION_PROBE_LOG"
test "${MISE_ENV+set}" = "" && test "$MISE_CONFIG_DIR" = "$2" &&
test "$MISE_OFFLINE:$MISE_SAFE:$MISE_NO_ENV:$MISE_NO_HOOKS:$MISE_AUTO_INSTALL:$MISE_EXEC_AUTO_INSTALL:$MISE_AUTO_UPDATE" = '1:1:1:1:0:0:0' &&
test "$MISE_STATE_DIR" != "$COMPLETION_ORIGINAL_STATE" &&
test "$MISE_CACHE_DIR" != "$COMPLETION_ORIGINAL_CACHE" || exit 3
printf '%s\n' "$COMPLETION_PROBE_OUTPUT"
`, 0700)
	return filepath.Join(probe, "mise")
}

func TestActiveRemoveAndOwnership(t *testing.T) {
	root := t.TempDir()
	// On macOS the temp directory can be reached through symlinked prefixes.
	root, err := filepath.EvalSymlinks(root)
	if err != nil {
		t.Fatal(err)
	}
	state := t.TempDir()
	t.Setenv("XDG_STATE_HOME", state)
	t.Setenv("MISE_ENV", "ambient")
	t.Setenv("MISE_STATE_DIR", state)
	t.Setenv("COMPLETION_ORIGINAL_STATE", state)
	cache := t.TempDir()
	t.Setenv("MISE_CACHE_DIR", cache)
	t.Setenv("COMPLETION_ORIGINAL_CACHE", cache)
	t.Setenv("DOTS_MISE_BIN", fixtureProbe(t))
	other := t.TempDir()
	fixture(t, other, "external.toml", "[tools]\nexternal = 'latest'\n", 0600)
	fixture(t, root, "config.toml", "[tools]\nnode = '22'\n", 0600)
	fixture(t, root, "config.server.toml", "[tools]\n'github:Owner/Widget' = 'latest'\n", 0600)
	fixture(t, root, "config.inactive.toml", "[tools]\ninactive = 'latest'\n", 0600)
	if err := os.Symlink(filepath.Join(other, "external.toml"), filepath.Join(root, "external.toml")); err != nil {
		t.Fatal(err)
	}
	files := []map[string]string{
		{"path": filepath.Join(root, "config.toml")},
		{"path": filepath.Join(root, "config.server.toml")},
		{"path": filepath.Join(root, "external.toml")},
	}
	list, _ := json.Marshal(files)
	t.Setenv("COMPLETION_PROBE_OUTPUT", string(list))
	oldDir := applicationsDir
	applicationsDir = t.TempDir()
	t.Cleanup(func() { applicationsDir = oldDir })
	app := func(repo, name, filename string) {
		if filename == "" {
			filename = fmt.Sprintf("%x.json", sha256.Sum256([]byte(name)))
		}
		data, _ := json.Marshal(map[string]string{"repo": repo, "path": filepath.Join(applicationsDir, name), "bundle_id": "test.example"})
		fixture(t, state, "dots/github-apps/"+filename, string(data), 0600)
	}
	app("owner/widget", "Dots Completion Fixture Widget.app", "")
	app("owner/standalone", "Dots Completion Fixture Standalone.app", "")
	app("owner/invalid", "Dots Completion Invalid.app", "invalid.json")
	expect(t, root, []string{"remove", "--d"}, []string{"--dry-run"})
	if _, err := os.Stat(os.Getenv("COMPLETION_PROBE_LOG")); !os.IsNotExist(err) {
		t.Fatal("static completion ran the active config probe")
	}
	expect(t, root, []string{"remove", ""}, []string{
		"dots completion fixture standalone", "dots completion fixture widget", "node",
		"owner/standalone", "owner/widget", "standalone", "widget",
	})
	expect(t, root, []string{"remove", "https://github.com/OWNER/widget.git", "Wid"}, nil)
	expect(t, root, []string{"remove", "--base", "stand"}, nil)
	expect(t, root, []string{"remove", "--base", ""}, []string{"node"})
	log, err := os.ReadFile(os.Getenv("COMPLETION_PROBE_LOG"))
	if err != nil || string(log) != "-C\n"+root+"\nconfig\nls\n--json\n" {
		t.Fatalf("unexpected probe call %q (%v)", log, err)
	}
}
