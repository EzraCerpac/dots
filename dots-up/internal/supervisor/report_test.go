package supervisor

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/EzraCerpac/dots/dots-up/internal/ui"
)

func TestRunReportsArePrivateAndRetentionPreservesActiveRun(t *testing.T) {
	root := t.TempDir()
	base := time.Date(2026, 10, 30, 12, 0, 0, 0, time.UTC)
	mode := func(path string) os.FileMode {
		t.Helper()
		info, err := os.Stat(path)
		if err != nil {
			t.Fatal(err)
		}
		return info.Mode().Perm()
	}
	assertMode := func(path string, want os.FileMode) {
		t.Helper()
		if got := mode(path); got != want {
			t.Fatalf("%s mode = %04o, want %04o", path, got, want)
		}
	}

	var first string
	for i := 0; i < 10; i++ {
		started := base.Add(time.Duration(i) * time.Second)
		files, err := newRunFiles(started, root)
		if err != nil {
			t.Fatal(err)
		}
		if i == 0 {
			first = files.runDir
			assertMode(root, 0o700)
			assertMode(files.runDir, 0o700)
			assertMode(filepath.Join(files.runDir, "stages"), 0o700)
			assertMode(filepath.Join(files.runDir, ".active"), 0o600)
			assertMode(filepath.Join(files.runDir, "events.jsonl"), 0o600)
		}
		if err := files.record(ui.Event{V: 1, Kind: "run_start"}); err != nil {
			t.Fatal(err)
		}
		if err := files.finish(0, "nas", "fixture", started.Add(time.Second)); err != nil {
			t.Fatal(err)
		}
		assertMode(filepath.Join(files.runDir, "report.json"), 0o600)
	}

	active, err := newRunFiles(base.Add(10*time.Second), root)
	if err != nil {
		t.Fatal(err)
	}
	latest, err := newRunFiles(base.Add(11*time.Second), root)
	if err != nil {
		t.Fatal(err)
	}
	if err := latest.finish(0, "nas", "fixture", base.Add(12*time.Second)); err != nil {
		t.Fatal(err)
	}

	entries, err := os.ReadDir(root)
	if err != nil {
		t.Fatal(err)
	}
	completed := 0
	for _, entry := range entries {
		path := filepath.Join(root, entry.Name())
		if _, err := os.Stat(filepath.Join(path, ".active")); err == nil {
			if path != active.runDir {
				t.Fatalf("unexpected active run %s", path)
			}
			continue
		}
		if _, err := os.Stat(filepath.Join(path, "report.json")); err == nil {
			completed++
		}
	}
	if completed != 10 {
		t.Fatalf("retained %d completed reports, want 10", completed)
	}
	if _, err := os.Stat(first); !os.IsNotExist(err) {
		t.Fatalf("oldest report still exists or could not be checked: %v", err)
	}
	if _, err := os.Stat(filepath.Join(active.runDir, ".active")); err != nil {
		t.Fatalf("active report was pruned: %v", err)
	}
	if err := active.eventLog.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestSummaryCountsAttentionStageResults(t *testing.T) {
	got := summary(Report{
		ExitCode: 0,
		Events:   []ui.Event{{V: 1, Kind: "stage_end", ID: "auth", Status: "attention"}},
	}, "/state/run")
	if !strings.Contains(got, "1 item(s) needing attention") {
		t.Fatalf("summary omitted the attention result: %s", got)
	}
}

func TestCommandSummaryNeedsAttentionDespiteExitZero(t *testing.T) {
	got := summary(Report{
		Command:  "sync",
		ExitCode: 0,
		Events:   []ui.Event{{V: 1, Kind: "notice", Severity: "attention", Summary: "NOT PUSHED: origin/dev kept moving; retried next run"}},
	}, "/state/run")
	if !strings.HasPrefix(got, "dots sync needs attention\n") {
		t.Fatalf("summary hid the attention result: %s", got)
	}
}

func TestReportHighlightsCarryEachIssuesOutputLogAndNextStep(t *testing.T) {
	dir := t.TempDir()
	log := filepath.Join(dir, "Breaks.log")
	if err := os.WriteFile(log, []byte("step one\nboom: disk full\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	var b strings.Builder
	printReportHighlights(&b, Report{Events: []ui.Event{
		{V: 1, Kind: "stage_start", ID: "Breaks", Label: "Breaks"},
		{V: 1, Kind: "stage_end", ID: "Breaks", Status: "failed", Summary: "failed (exit 7)", Action: "Fix the error above, then run dots up", LogPath: log},
	}}, dir)
	for _, want := range []string{
		"    × Breaks — failed (exit 7)",
		"        step one",
		"        boom: disk full",
		"        Next: Fix the error above, then run dots up",
		"        Log: " + log,
	} {
		if !strings.Contains(b.String(), want+"\n") {
			t.Fatalf("highlights lack %q:\n%s", want, b.String())
		}
	}
}
