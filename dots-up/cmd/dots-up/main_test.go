package main

import (
	"io"
	"os"
	"strings"
	"testing"
)

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
