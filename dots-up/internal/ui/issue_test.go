package ui

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
)

// The sync stage as dots up reports it: one attention stage that carries the
// failed command's output and the command to run next; its log holds the block.
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
		Detail:  "mise $ dots internal persist-selected\nmise ERROR pre-dotfiles hook failed: github:EzraCerpac/dots@0.3.1 not found",
		Action:  "dots apply"})
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
		"    mise ERROR pre-dotfiles hook failed: github:EzraCerpac/dots@0.3.1 not found",
		"    Next: dots apply",
		"    Log: " + log,
	} {
		if !strings.Contains(m.lastLog, want) {
			t.Fatalf("issue details lack %q:\n%s", want, m.lastLog)
		}
	}
	if strings.Contains(m.lastLog, "dots sync: no local changes") {
		t.Fatalf("issue details repeat the log instead of the error:\n%s", m.lastLog)
	}
	if !strings.Contains(m.View().Content, "A agent") {
		t.Fatal("footer does not offer the agent hand-off")
	}
}

// Without pressing anything, the attention list shows each problem with its
// error and the command to run next, on screen and in the printed report.
func TestNeedsAttentionListShowsTheErrorAndTheNextCommand(t *testing.T) {
	m, _ := syncAttention(t)
	m = send(m, Event{V: 1, Kind: "notice", Stage: "source-sync", Severity: "attention", Summary: "LATE EDIT: config.toml not updated; rerun", Action: "dots sync"})
	m = send(m, Event{V: 1, Kind: "run_end"})
	m.Width, m.Height = 120, 40
	m.resize()
	m.refreshDetail()
	events := []Event{
		{V: 1, Kind: "stage_start", ID: "source-sync", Group: "Source & settings", Label: "Sync configuration"},
		{V: 1, Kind: "stage_end", ID: "source-sync", Status: "attention", Summary: m.Stages[0].Summary, Detail: m.Stages[0].Detail, Action: "dots apply"},
		{V: 1, Kind: "notice", Stage: "source-sync", Severity: "attention", Summary: "LATE EDIT: config.toml not updated; rerun", Action: "dots sync"},
		{V: 1, Kind: "run_end"},
	}
	for name, text := range map[string]string{"dashboard": m.View().Content, "report": OutcomeReport(events, "")} {
		for _, want := range []string{
			"Needs attention · 2",
			"! Sync configuration — APPLY FAILED: mise bootstrap dotfiles apply (exit 1)",
			"        mise ERROR pre-dotfiles hook failed: github:EzraCerpac/dots@0.3.1 not found",
			"        Next: dots apply",
			"! Sync configuration — LATE EDIT: config.toml not updated; rerun",
			"        Next: dots sync",
		} {
			if !strings.Contains(text, want) {
				t.Fatalf("%s lacks %q:\n%s", name, want, text)
			}
		}
		if strings.Contains(text, "open for details") || name == "report" && strings.Contains(text, "Packages · no verified changes") {
			t.Fatalf("%s keeps an element that does not help:\n%s", name, text)
		}
	}
}

func TestNarrowDashboardWrapsTheErrorInsteadOfCuttingIt(t *testing.T) {
	m, _ := syncAttention(t)
	m = send(m, Event{V: 1, Kind: "run_end"})
	updated, _ := m.Update(tea.WindowSizeMsg{Width: 60, Height: 40})
	content := updated.(Model).View().Content
	if !strings.Contains(content, "@0.3.1 not found") || !strings.Contains(content, "Next: dots apply") {
		t.Fatalf("the error was cut:\n%s", content)
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
		"APPLY FAILED", "mise ERROR pre-dotfiles hook failed", "Log: " + log,
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

func TestAgentMessageShowsFromTheIssueListAndTheLog(t *testing.T) {
	t.Setenv(AgentCommandEnv, "")
	m, _ := syncAttention(t)
	m = send(m, Event{V: 1, Kind: "run_end"})
	m = press(m, tea.KeyPressMsg{Code: tea.KeyUp})
	m = press(m, tea.KeyPressMsg{Code: tea.KeyEnter}) // the issue list, details closed
	for _, setup := range []func(Model) Model{
		func(m Model) Model { return m },
		func(m Model) Model { return press(m, tea.KeyPressMsg{Code: 'l', Text: "l"}) },
	} {
		updated, _ := setup(m).Update(tea.KeyPressMsg{Code: 'A', Text: "A"})
		if got := updated.(Model).lastLog; !strings.Contains(got, "Set DOTS_AGENT_COMMAND") {
			t.Fatalf("agent message hidden:\n%s", got)
		}
	}
}

func TestAnItemWithoutOutputDoesNotBorrowAnotherProblemsLog(t *testing.T) {
	m, log := syncAttention(t)
	m = send(m, Event{V: 1, Kind: "stage_end", ID: "source-sync", Status: "attention", LogPath: log,
		Summary: "SAVE REFUSED: history is not enrolled; nothing saved", Action: "dots sync"})
	m = send(m, Event{V: 1, Kind: "notice", Stage: "source-sync", Severity: "attention",
		Summary: "APPLY FAILED: mise bootstrap dotfiles apply (exit 1)",
		Detail:  "mise ERROR pre-dotfiles hook failed", Action: "dots apply"})
	items := m.issues()
	if len(items) != 2 {
		t.Fatalf("issues = %d, want 2", len(items))
	}
	if got := m.issueDetail(items[0]); strings.Contains(got, "mise ERROR") {
		t.Fatalf("the refusal shows the apply failure's output:\n%s", got)
	}
	if got := m.issueDetail(items[1]); !strings.Contains(got, "mise ERROR pre-dotfiles hook failed") {
		t.Fatalf("the apply failure lost its output:\n%s", got)
	}
}
