package supervisor

import (
	"encoding/json"
	"errors"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/EzraCerpac/dots/dots-up/internal/ui"
)

func TestDashboardFailureContinuesOneUpdaterRun(t *testing.T) {
	oldInteractive, oldSize := hasInteractiveTerminal, currentTerminalSize
	oldStart, oldDashboard := startUpdaterBackend, runDashboard
	defer func() {
		hasInteractiveTerminal, currentTerminalSize = oldInteractive, oldSize
		startUpdaterBackend, runDashboard = oldStart, oldDashboard
	}()

	hasInteractiveTerminal = func() bool { return true }
	currentTerminalSize = func() (int, int) { return 100, 30 }
	starts := 0
	startUpdaterBackend = func(Options, *runFiles, bool) (*backend, *os.File, io.Reader, error) {
		starts++
		eventR, eventW, err := os.Pipe()
		if err != nil {
			return nil, nil, nil, err
		}
		cmd := exec.Command("sh", "-c", "sleep 0.05")
		if err := cmd.Start(); err != nil {
			_ = eventR.Close()
			_ = eventW.Close()
			return nil, nil, nil, err
		}
		go func() {
			defer eventW.Close()
			for _, event := range []ui.Event{
				{V: 1, Kind: "run_start", Profile: "nas", Host: "fixture"},
				{V: 1, Kind: "stage_start", ID: "tools", Group: "Tools", Label: "Mise tools"},
				{V: 1, Kind: "stage_end", ID: "tools", Status: "ok", Summary: "completed"},
				{V: 1, Kind: "run_end", ExitCode: 0},
			} {
				if err := json.NewEncoder(eventW).Encode(event); err != nil {
					return
				}
			}
		}()
		return &backend{command: cmd, pty: true}, eventR, nil, nil
	}
	runDashboard = func(<-chan ui.Event, io.Writer, string, bool) error {
		return errors.New("injected renderer failure")
	}

	state := filepath.Join(t.TempDir(), "state")
	if code := Run(Options{Root: t.TempDir(), StateDir: state}); code != 0 {
		t.Fatalf("run returned %d after renderer failure", code)
	}
	if starts != 1 {
		t.Fatalf("backend started %d times after renderer failure, want exactly once", starts)
	}
	report, _, err := latestReport(state)
	if err != nil {
		t.Fatal(err)
	}
	if report.ExitCode != 0 || len(report.Events) < 4 {
		t.Fatalf("updater did not complete and save its report after renderer failure: %+v", report)
	}
}

func TestUpdaterCommandRunsManagerFromHome(t *testing.T) {
	t.Setenv("DOTS_UP_RUNNER", "")
	t.Setenv("DOTS_MISE_BIN", "/fixture/mise")
	if _, err := updaterCommand(Options{Root: "/fixture/config", Command: "status"}); err == nil {
		t.Fatal("a command started without a manager directory")
	}
	up, err := updaterCommand(Options{Root: "/fixture/config", Home: "/fixture/manager", Args: []string{"--plain"}})
	if err != nil {
		t.Fatal(err)
	}
	if got := strings.Join(up.Args, " "); got != "bash /fixture/manager/bin/dots --backend up --plain" {
		t.Fatalf("up runs %q", got)
	}
	status, err := updaterCommand(Options{Root: "/fixture/config", Home: "/fixture/manager", Command: "status"})
	if err != nil {
		t.Fatal(err)
	}
	if got := strings.Join(status.Args, " "); got != "bash /fixture/manager/lib/dots-command-backend /fixture/mise /fixture/config status" {
		t.Fatalf("status runs %q", got)
	}
}

// A dots script from before DOTS_HOME runs the newest renderer for `dots up`
// with only --root. The renderer must keep using that script's updater task.
func TestUpdaterCommandWithoutHomeUsesLegacySetupTask(t *testing.T) {
	t.Setenv("DOTS_UP_RUNNER", "")
	t.Setenv("DOTS_MISE_BIN", "/fixture/mise")
	for _, command := range []string{"", "up"} {
		up, err := updaterCommand(Options{Root: "/fixture/config", Command: command, Args: []string{"--plain"}})
		if err != nil {
			t.Fatal(err)
		}
		if got := strings.Join(up.Args, " "); got != "/fixture/mise -C /fixture/config run setup:update --plain" {
			t.Fatalf("legacy up (%q) runs %q", command, got)
		}
	}
}

func TestRedirectedResizeAndInterrupt(t *testing.T) {
	for _, tc := range []struct {
		signal string
		code   int
	}{
		{"WINCH", 0},
		{"INT", 130},
		{"TERM", 130},
	} {
		t.Run(tc.signal, func(t *testing.T) {
			useTextRunner(t, "kill -"+tc.signal+" \"$PPID\"\nsleep 0.3\n")
			code, _ := captureRun(t, Options{StateDir: t.TempDir()})
			if code != tc.code {
				t.Fatalf("got exit %d after %s, want %d", code, tc.signal, tc.code)
			}
		})
	}
}

func TestStartupFailureReportsOneIssue(t *testing.T) {
	for _, tc := range []struct {
		name, command, script, diagnostic string
		code                              int
	}{
		{"launch", "up", "", "no such file or directory", 127},
		{"before run start", "up", "echo 'setup: invalid profile' >&2\nexit 2", "setup: invalid profile", 2},
		{"before first stage", "status", runnerEvents + `
emit '{"v":1,"kind":"run_start","profile":"invalid","host":"fixture"}'
echo 'setup: invalid profile' >&2
emit '{"v":1,"kind":"run_end","status":"failed","exit_code":2}'
exit 2
`, "setup: invalid profile", 2},
	} {
		t.Run(tc.name, func(t *testing.T) {
			useTextRunner(t, tc.script)
			state := t.TempDir()
			code, output := captureRun(t, Options{Command: tc.command, StateDir: state})
			if code != tc.code || strings.Count(output, tc.diagnostic) != 1 ||
				!strings.Contains(output, "Next:") || !strings.Contains(output, "Log:") {
				t.Fatalf("exit %d, expected %d and one actionable diagnostic:\n%s", code, tc.code, output)
			}
			report, dir, err := latestReport(state)
			if err != nil {
				t.Fatal(err)
			}
			events := report.Events
			if report.ExitCode != code || len(events) < 2 || events[len(events)-1].Kind != "run_end" {
				t.Fatalf("report did not finish: %+v", report)
			}
			var failures int
			for _, e := range events {
				if e.Kind == "stage_end" && e.Status == "failed" {
					failures++
					if !strings.Contains(e.Detail, tc.diagnostic) || e.LogPath != filepath.Join(dir, "backend.log") {
						t.Fatalf("issue lost its diagnostic or log: %+v", e)
					}
				}
			}
			log, err := os.ReadFile(filepath.Join(dir, "backend.log"))
			if failures != 1 || err != nil || !strings.Contains(string(log), tc.diagnostic) {
				t.Fatalf("issues = %d, log = %q, error = %v", failures, log, err)
			}
		})
	}
}

func TestStatusPrintsStructuredInventoryInTextModes(t *testing.T) {
	useTextRunner(t, runnerEvents+`
emit '{"v":1,"kind":"run_start","profile":"laptop","host":"fixture-nas"}'
emit '{"v":1,"kind":"item_upsert","label":"sample-tool@1","group":"Tools","status":"ok","summary":"1.2.3"}'
emit '{"v":1,"kind":"item_upsert","label":"brew:sample-package","group":"Packages","status":"ok","summary":"2.3.4"}'
emit '{"v":1,"kind":"item_upsert","label":"missing-tool","group":"Tools","status":"missing","summary":"not available"}'
emit '{"v":1,"kind":"run_end","exit_code":0}'
`)
	for _, opts := range []Options{{}, {Plain: true}, {Verbose: true}} {
		opts.Command, opts.StateDir = "status", t.TempDir()
		code, output := captureRun(t, opts)
		if code != 0 {
			t.Fatalf("status exited %d:\n%s", code, output)
		}
		for _, want := range []string{"laptop · fixture-nas", "sample-tool@1  ok — 1.2.3", "brew:sample-package  ok — 2.3.4", "missing-tool  missing"} {
			if strings.Count(output, want) != 1 {
				t.Fatalf("expected one %q in status output:\n%s", want, output)
			}
		}
	}
}

const runnerEvents = `
emit() { printf '\036dots-up/1:%s:%s\037' "$DOTS_UP_STREAM_NONCE" "$1"; }
`

func useTextRunner(t *testing.T, script string) {
	t.Helper()
	t.Setenv("CI", "1")
	runner := filepath.Join(t.TempDir(), "runner")
	t.Setenv("DOTS_UP_RUNNER", runner)
	if script != "" {
		if err := os.WriteFile(runner, []byte("#!/bin/sh\n"+script+"\n"), 0o700); err != nil {
			t.Fatal(err)
		}
	}
}

func captureRun(t *testing.T, opts Options) (int, string) {
	t.Helper()
	output, err := os.CreateTemp(t.TempDir(), "output")
	if err != nil {
		t.Fatal(err)
	}
	defer output.Close()
	stdout, stderr := os.Stdout, os.Stderr
	defer func() { os.Stdout, os.Stderr = stdout, stderr }()
	os.Stdout, os.Stderr = output, output
	code := Run(opts)
	data, err := os.ReadFile(output.Name())
	if err != nil {
		t.Fatal(err)
	}
	return code, string(data)
}
