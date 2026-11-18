package main

import (
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestDirectCommandsHonorSelectedRootAndHomeOverAmbient(t *testing.T) {
	for _, ambient := range []string{"unset", "conflicting"} {
		for _, command := range []string{"plan"} {
			t.Run(command+"/"+ambient, func(t *testing.T) {
				base, err := filepath.EvalSymlinks(t.TempDir())
				if err != nil {
					t.Fatal(err)
				}
				selected := filepath.Join(base, "selected")
				other := filepath.Join(base, "ambient")
				for _, root := range []string{selected, other} {
					if err := os.MkdirAll(root, 0700); err != nil {
						t.Fatal(err)
					}
					if err := os.WriteFile(filepath.Join(root, "config.toml"), []byte("# fixture\n"), 0600); err != nil {
						t.Fatal(err)
					}
				}
				// The manager lives apart from both configurations.
				home := filepath.Join(base, "manager")
				otherHome := filepath.Join(base, "ambient-manager")
				for _, name := range []string{"bin/dots", "lib/dots-packages.sh"} {
					source, err := os.ReadFile(filepath.Join("..", "..", "..", name))
					if err != nil {
						t.Fatal(err)
					}
					target := filepath.Join(home, name)
					if err := os.MkdirAll(filepath.Dir(target), 0700); err != nil {
						t.Fatal(err)
					}
					if err := os.WriteFile(target, source, 0700); err != nil {
						t.Fatal(err)
					}
				}
				log := filepath.Join(base, "mise-args")
				mise := filepath.Join(base, "mise")
				if err := os.WriteFile(mise, []byte(`#!/bin/sh
printf '%s\n' "$MISE_CONFIG_DIR" "$@" >>"$DIRECT_TEST_LOG"
if [ "$3 $4 $5" = 'bootstrap packages status' ]; then
    printf '{}\n'
fi
`), 0700); err != nil {
					t.Fatal(err)
				}
				t.Setenv("DOTS_MISE_BIN", mise)
				t.Setenv("DIRECT_TEST_LOG", log)
				t.Setenv("HOME", base)
				t.Setenv("DOTS_ROOT", other)
				t.Setenv("DOTS_HOME", otherHome)
				if ambient == "unset" {
					for _, key := range []string{"DOTS_ROOT", "DOTS_HOME"} {
						if err := os.Unsetenv(key); err != nil {
							t.Fatal(err)
						}
					}
				}
				args := []string{"--root", selected, "--home", home, "--command", command, "--"}
				if code := run(args); code != 0 {
					t.Fatalf("direct command exited %d", code)
				}
				output, err := os.ReadFile(log)
				if err != nil {
					t.Fatal(err)
				}
				want := strings.Join([]string{
					selected, "-C", selected, "bootstrap", "packages", "status", "--json",
					selected, "-C", selected, "bootstrap", "plan",
				}, "\n") + "\n"
				if string(output) != want {
					t.Fatalf("mise invocations = %q, want %q", output, want)
				}
			})
		}
	}
}

func TestCapabilitiesReportsHomeAwareProtocol(t *testing.T) {
	stdout := os.Stdout
	r, w, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	os.Stdout = w
	code := run([]string{"--capabilities"})
	os.Stdout = stdout
	_ = w.Close()
	output, err := io.ReadAll(r)
	if err != nil {
		t.Fatal(err)
	}
	if code != 0 || strings.TrimSpace(string(output)) != "dots-shared-cli/3" {
		t.Fatalf("capabilities = %q (exit %d)", output, code)
	}
}

func TestSelectedRootAndHomeReachTheDashboardProcess(t *testing.T) {
	root, home := t.TempDir(), t.TempDir()
	t.Setenv("DOTS_ROOT", "/ambient/config")
	t.Setenv("DOTS_HOME", "/ambient/manager")
	if code := run([]string{"--root", root, "--home", home, "--command", "no-such-command", "--"}); code != 2 {
		t.Fatalf("run = %d, want 2 for an unknown command", code)
	}
	if os.Getenv("DOTS_ROOT") != root || os.Getenv("DOTS_HOME") != home {
		t.Fatalf("DOTS_ROOT=%q DOTS_HOME=%q, want the selected %q and %q", os.Getenv("DOTS_ROOT"), os.Getenv("DOTS_HOME"), root, home)
	}
}
