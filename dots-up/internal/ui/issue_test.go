package ui

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
)

// The sync stage as dots up reports it: one attention stage whose log holds
// the block, with the failed command's output under its line.
func syncAttention(t *testing.T) (Model, string) {
	t.Helper()
	dir := t.TempDir()
	log := filepath.Join(dir, "stages", "source-sync.log")
	if err := os.MkdirAll(filepath.Dir(log), 0o700); err != nil {
		t.Fatal(err)
	}
	block := "dots sync: no local changes · pulled dev +1, main +0 · nothing to push\n" +
		"  APPLY FAILED: mise bootstrap dotfiles apply (exit 1); fix the error below, then run dots apply\n" +
		"    mise ERROR pre-dotfiles hook failed: github:EzraCerpac/dots@0.3.1 not found\n"
	if err := os.WriteFile(log, []byte(block), 0o600); err != nil {
		t.Fatal(err)
	}
	m := NewModel(dir, false)
	m = send(m, Event{V: 1, Kind: "run_start", Host: "mac", Profile: "laptop"})
	m = send(m, Event{V: 1, Kind: "stage_start", ID: "source-sync", Group: "Source & settings", Label: "Sync configuration", LogPath: log})
	m = send(m, Event{V: 1, Kind: "stage_end", ID: "source-sync", Status: "attention", LogPath: log,
		Summary: "APPLY FAILED: mise bootstrap dotfiles apply (exit 1); fix the error below, then run dots apply",
		Action:  "Fix what the sync output says, then run dots sync"})
	return m, log
}

func openFirstIssue(m Model) Model {
	m = press(m, tea.KeyPressMsg{Code: tea.KeyUp})
	m = press(m, tea.KeyPressMsg{Code: tea.KeyEnter})
	return press(m, tea.KeyPressMsg{Code: tea.KeyEnter})
}

func TestIssueDetailsShowTheErrorTheLogAndTheNextStep(t *testing.T) {
	m, log := syncAttention(t)
	m = send(m, Event{V: 1, Kind: "run_end"})
	if got := len(m.issues()); got != 1 {
		t.Fatalf("issues = %d, want the sync problem once", got)
	}
	m = openFirstIssue(m)
	for _, want := range []string{
		"! Sync configuration — APPLY FAILED: mise bootstrap dotfiles apply (exit 1)",
		"  mise ERROR pre-dotfiles hook failed: github:EzraCerpac/dots@0.3.1 not found",
		"Full log: " + log,
		"Next: Fix what the sync output says, then run dots sync",
	} {
		if !strings.Contains(m.lastLog, want) {
			t.Fatalf("issue details lack %q:\n%s", want, m.lastLog)
		}
	}
	if !strings.Contains(m.View().Content, "A agent") {
		t.Fatal("footer does not offer the agent hand-off")
	}
}

func TestAgentHandOffWaitsForTheRunAndNeedsACommand(t *testing.T) {
	t.Setenv(AgentCommandEnv, "")
	m, _ := syncAttention(t)
	m = openFirstIssue(m)
	updated, cmd := m.Update(tea.KeyPressMsg{Code: 'A', Text: "A"})
	m = updated.(Model)
	if cmd != nil || !strings.Contains(m.lastLog, "once this run finishes") {
		t.Fatalf("hand-off ran during the update:\n%s", m.lastLog)
	}
	m = send(m, Event{V: 1, Kind: "run_end"})
	updated, cmd = m.Update(tea.KeyPressMsg{Code: 'A', Text: "A"})
	m = updated.(Model)
	if cmd != nil || !strings.Contains(m.lastLog, "Set DOTS_AGENT_COMMAND") {
		t.Fatalf("unset command was not explained:\n%s", m.lastLog)
	}
	t.Setenv(AgentCommandEnv, "claude")
	if _, cmd = m.Update(tea.KeyPressMsg{Code: 'A', Text: "A"}); cmd == nil {
		t.Fatal("a configured agent command was not started")
	}
}

func TestAgentGetsTheIssueAsItsLastArgument(t *testing.T) {
	root, home := t.TempDir(), t.TempDir()
	if err := os.WriteFile(filepath.Join(home, "VERSION"), []byte("0.3.1\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	t.Setenv("DOTS_ROOT", root)
	t.Setenv("DOTS_HOME", home)
	m, log := syncAttention(t)
	item := m.issues()[0]
	prompt := m.agentPrompt(item)
	for _, want := range []string{
		"APPLY FAILED", "mise ERROR pre-dotfiles hook failed", "Full log: " + log,
		"Configuration root (DOTS_ROOT): " + root, "Host: mac, profile: laptop", "dots 0.3.1",
	} {
		if !strings.Contains(prompt, want) {
			t.Fatalf("prompt lacks %q:\n%s", want, prompt)
		}
	}
	cmd := agentCommand(`printf '%s|' "$(pwd -P)"; printf '%s'`, prompt)
	out, err := cmd.Output()
	if err != nil {
		t.Fatal(err)
	}
	realRoot, _ := filepath.EvalSymlinks(root)
	if string(out) != realRoot+"|"+prompt {
		t.Fatalf("agent got %q", out)
	}
}
