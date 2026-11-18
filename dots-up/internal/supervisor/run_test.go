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
			if code != tc.code || strings.Count(output, tc.diagnostic) != 1 {
				t.Fatalf("exit %d, expected %d and one actionable diagnostic:\n%s", code, tc.code, output)
			}
			if tc.command == "up" && (!strings.Contains(output, "Next:") || !strings.Contains(output, "Log:")) {
				t.Fatalf("up lost its startup issue details:\n%s", output)
			}
			if tc.command == "status" && output != "dots status · invalid · fixture\n"+tc.diagnostic+"\n" &&
				output != tc.diagnostic+"\ndots status · invalid · fixture\n" {
				t.Fatalf("status added unnecessary output:\n%s", output)
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
					if tc.command == "up" && (!strings.Contains(e.Detail, tc.diagnostic) || e.LogPath != filepath.Join(dir, "backend.log")) {
						t.Fatalf("issue lost its diagnostic or log: %+v", e)
					}
				}
			}
			log, err := os.ReadFile(filepath.Join(dir, "backend.log"))
			if failures != 1 || (tc.command == "up" && (err != nil || !strings.Contains(string(log), tc.diagnostic))) {
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
emit '{"v":1,"kind":"item_upsert","label":"repository","group":"Source","status":"attention","summary":"Git on dev","action":"Legacy sync state; see docs/sync.md"}'
emit '{"v":1,"kind":"item_upsert","label":"missing-tool","group":"Tools","status":"missing","summary":"not available"}'
emit '{"v":1,"kind":"item_upsert","label":"settings history","group":"History","status":"skipped","summary":"disabled"}'
emit '{"v":1,"kind":"run_end","exit_code":0}'
`)
	expect := []string{"laptop · fixture-nas", "sample-tool@1  ok — 1.2.3", "brew:sample-package  ok — 2.3.4", "missing-tool  missing", "settings history  skipped", "repository  attention — Git on dev\n    Legacy sync state; see docs/sync.md"}
	for _, opts := range []Options{{}, {Plain: true}, {Verbose: true}} {
		opts.Command, opts.StateDir = "status", t.TempDir()
		code, output := captureRun(t, opts)
		if code != 0 {
			t.Fatalf("status exited %d:\n%s", code, output)
		}
		assertStatusInventory(t, output, expect...)
		if opts.Plain {
			code, output = captureOutput(t, func() int { return ShowLast(opts) })
			if code != 0 {
				t.Fatalf("status replay exited %d:\n%s", code, output)
			}
			assertStatusInventory(t, output, expect...)
		}
	}
}

func TestFailedUpdateConfigShowsDiagnosticWhenReportsAreUnavailable(t *testing.T) {
	const diagnostic = "mise: invalid disposable configuration"
	useTextRunner(t, runnerEvents+`
emit '{"v":1,"kind":"run_start","profile":"laptop","host":"fixture"}'
emit '{"v":1,"kind":"stage_start","id":"update-config","label":"Validate update configuration"}'
printf '%s\n' '`+diagnostic+`' >&2
emit '{"v":1,"kind":"stage_end","id":"update-config","status":"failed","summary":"Cannot load configuration","action":"Fix the problem and rerun dots up."}'
exit 1
`)
	state := filepath.Join(t.TempDir(), "not-a-directory")
	if err := os.WriteFile(state, nil, 0o600); err != nil {
		t.Fatal(err)
	}
	code, output := captureRun(t, Options{StateDir: state, Plain: true})
	if code != 1 || !strings.Contains(output, "reports unavailable") || strings.Count(output, diagnostic) != 1 ||
		!strings.Contains(output, "Next: Fix the problem and rerun dots up.") || strings.Contains(output, "Log:") {
		t.Fatalf("expected one actionable diagnostic without a log, got exit %d:\n%s", code, output)
	}

	t.Run("only stage log unavailable", func(t *testing.T) {
		start := startUpdaterBackend
		t.Cleanup(func() { startUpdaterBackend = start })
		startUpdaterBackend = func(opts Options, files *runFiles, interactive bool) (*backend, *os.File, io.Reader, error) {
			if err := os.Mkdir(files.logPath("update-config"), 0o700); err != nil {
				return nil, nil, nil, err
			}
			return start(opts, files, interactive)
		}
		state := t.TempDir()
		code, output := captureRun(t, Options{StateDir: state, Plain: true})
		if code != 1 || strings.Count(output, diagnostic) != 1 || strings.Count(output, "Next: Fix the problem and rerun dots up.") != 1 ||
			strings.Contains(output, "Log:") {
			t.Fatalf("expected one actionable failure without a log, got exit %d:\n%s", code, output)
		}
		report, _, err := latestReport(state)
		if err != nil {
			t.Fatal(err)
		}
		for _, event := range report.Events {
			if event.ID == "update-config" && (event.Detail != "" || event.LogPath != "") {
				t.Fatalf("saved config event includes a transient detail or log path: %+v", event)
			}
		}
	})

	state = t.TempDir()
	code, output = captureRun(t, Options{StateDir: state, Plain: true, Sensitive: true})
	if code != 1 || strings.Contains(output, diagnostic) {
		t.Fatalf("sensitive run exposed its diagnostic, exit %d:\n%s", code, output)
	}
	report, _, err := latestReport(state)
	if err != nil {
		t.Fatal(err)
	}
	for _, event := range report.Events {
		if event.Detail != "" || event.LogPath != "" {
			t.Fatalf("sensitive report saved a detail or log path: %+v", event)
		}
	}
}

func TestInteractiveStatusPrintsInventoryWithoutDashboard(t *testing.T) {
	useTextRunner(t, runnerEvents+`
emit '{"v":1,"kind":"run_start","profile":"laptop","host":"fixture-nas"}'
emit '{"v":1,"kind":"item_upsert","id":"first","label":"earlier-tool","group":"Tools","status":"missing"}'
emit '{"v":1,"kind":"item_upsert","id":"later","label":"later-package","group":"Packages","status":"ok"}'
`)
	oldInteractive, oldSize := hasInteractiveTerminal, currentTerminalSize
	oldDashboard := runDashboard
	t.Cleanup(func() {
		hasInteractiveTerminal, currentTerminalSize = oldInteractive, oldSize
		runDashboard = oldDashboard
	})
	hasInteractiveTerminal = func() bool { return true }
	currentTerminalSize = func() (int, int) { return 100, 30 }
	runDashboard = func(<-chan ui.Event, io.Writer, string, bool) error {
		t.Error("status started a dashboard")
		return nil
	}
	code, output := captureRun(t, Options{Command: "status", StateDir: t.TempDir()})
	if code != 0 {
		t.Fatalf("status exited %d:\n%s", code, output)
	}
	assertStatusInventory(t, output, "laptop · fixture-nas", "earlier-tool  missing", "later-package  ok")
}

func assertStatusInventory(t *testing.T, output string, expected ...string) {
	t.Helper()
	for _, want := range expected {
		if strings.Count(output, want) != 1 {
			t.Fatalf("expected one %q in status output:\n%s", want, output)
		}
	}
}

const runnerEvents = `
emit() {
    if [ -n "$DOTS_UP_EVENT_FD" ]; then
        printf '%s\n' "$1" >&3
    else
        printf '\036dots-up/1:%s:%s\037' "$DOTS_UP_STREAM_NONCE" "$1"
    fi
}
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
	return captureOutput(t, func() int { return Run(opts) })
}

func captureOutput(t *testing.T, run func() int) (int, string) {
	t.Helper()
	output, err := os.CreateTemp(t.TempDir(), "output")
	if err != nil {
		t.Fatal(err)
	}
	defer output.Close()
	stdout, stderr := os.Stdout, os.Stderr
	defer func() { os.Stdout, os.Stderr = stdout, stderr }()
	os.Stdout, os.Stderr = output, output
	code := run()
	data, err := os.ReadFile(output.Name())
	if err != nil {
		t.Fatal(err)
	}
	return code, string(data)
}

func TestInteractiveSyncShowsOneReceipt(t *testing.T) {
	oldInteractive, oldStart := hasInteractiveTerminal, startUpdaterBackend
	t.Cleanup(func() { hasInteractiveTerminal, startUpdaterBackend = oldInteractive, oldStart })
	hasInteractiveTerminal = func() bool { return true }
	startUpdaterBackend = func(opts Options, files *runFiles, _ bool) (*backend, *os.File, io.Reader, error) {
		return startBackend(opts, files, false)
	}
	const staged = runnerEvents + `
emit '{"v":1,"kind":"stage_start","id":"sync","label":"sync"}'
printf 'Plain receipt from this sync\n'
emit '{"v":1,"kind":"stage_end","id":"sync","status":"ok"}'
`
	for _, tc := range []struct {
		name, script, preview, want string
		code                        int
	}{
		{"stage log", staged, "exit 1", "Plain receipt from this sync", 0},
		{"early startup", "echo 'Invalid sync profile' >&2\nexit 2", "exit 1", "Invalid sync profile", 2},
		{"preview succeeds", staged, "echo 'Rendered current receipt'", "Rendered current receipt", 0},
	} {
		t.Run(tc.name, func(t *testing.T) {
			useTextRunner(t, tc.script)
			home := t.TempDir()
			bin := filepath.Join(home, "bin")
			if err := os.Mkdir(bin, 0o700); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(filepath.Join(bin, "dots"), []byte(tc.preview), 0o600); err != nil {
				t.Fatal(err)
			}
			state := t.TempDir()
			code, output := captureRun(t, Options{Command: "sync", Home: home, StateDir: state, Plain: true})
			if code != tc.code || output != tc.want+"\n" {
				t.Fatalf("exit %d, output %q, want exit %d and one %q", code, output, tc.code, tc.want)
			}
			if tc.name == "stage log" {
				_, dir, err := latestReport(state)
				if err != nil {
					t.Fatal(err)
				}
				stageLog, err := os.ReadFile(filepath.Join(dir, "stages", "sync.log"))
				if err != nil || string(stageLog) != output {
					t.Fatalf("receipt did not come from staged output: log %q, error %v", stageLog, err)
				}
			}
		})
	}
}

func TestUpLiveSummarySurvivesEventLogFailure(t *testing.T) {
	useTextRunner(t, runnerEvents+`
emit '{"v":1,"kind":"stage_start","id":"tools","group":"Tools","label":"Update tools"}'
emit '{"v":1,"kind":"change","stage":"tools","item":"fixture-tool","action":"installed","after":"1.2.3"}'
emit '{"v":1,"kind":"stage_end","id":"tools","status":"ok"}'
`)
	oldStart := startUpdaterBackend
	t.Cleanup(func() { startUpdaterBackend = oldStart })
	startUpdaterBackend = func(opts Options, files *runFiles, interactive bool) (*backend, *os.File, io.Reader, error) {
		if err := files.eventLog.Close(); err != nil {
			t.Fatal(err)
		}
		return startBackend(opts, files, interactive)
	}
	code, output := captureRun(t, Options{StateDir: t.TempDir(), Plain: true})
	if code != 0 || strings.Count(output, "installed  fixture-tool  1.2.3") != 1 ||
		!strings.Contains(output, "write update event log") || strings.Contains(output, "dots up --last") {
		t.Fatalf("exit %d, expected one verified change and no replay hint:\n%s", code, output)
	}
}

func TestPlainUpPrintsOneCompactSummaryAndReplaysIt(t *testing.T) {
	const change = "Shared on dev · edited config.toml (+1 −1)"
	useTextRunner(t, runnerEvents+`
emit '{"v":1,"kind":"run_start","profile":"laptop","host":"fixture"}'
emit '{"v":1,"kind":"stage_start","id":"source-sync","group":"Source & settings","label":"Sync configuration"}'
emit '{"v":1,"kind":"change","stage":"source-sync","item":"`+change+`","action":"changed"}'
emit '{"v":1,"kind":"stage_end","id":"source-sync","status":"ok","summary":"Source sync succeeded"}'
emit '{"v":1,"kind":"stage_start","id":"tools","group":"Tools","label":"Update tools"}'
emit '{"v":1,"kind":"stage_end","id":"tools","status":"attention","summary":"Tool upgrade deferred","action":"Free up disk space and rerun dots up"}'
emit '{"v":1,"kind":"app_store_end","id":"mac-app-store","status":"failed","summary":"could not start (exit 1)","exit_code":1}'
emit '{"v":1,"kind":"run_end","exit_code":0}'
`)
	opts := Options{StateDir: t.TempDir(), Plain: true}
	code, live := captureRun(t, opts)
	replayCode, replay := captureOutput(t, func() int { return ShowLast(opts) })
	if code != 0 || replayCode != 0 {
		t.Fatalf("live exit %d, replay exit %d", code, replayCode)
	}
	for _, output := range []string{live, replay} {
		for _, want := range []string{"dots up · Completed with issues", change, "Tool upgrade deferred", "Next: Free up disk space", "Mac App Store updates — could not start (exit 1)", "rerun dots up --mas"} {
			if strings.Count(output, want) != 1 {
				t.Fatalf("expected %q once:\n%s", want, output)
			}
		}
		for _, hidden := range []string{"changed  " + change, "Source sync succeeded", "Report:", "no verified changes"} {
			if strings.Contains(output, hidden) {
				t.Fatalf("unexpected %q:\n%s", hidden, output)
			}
		}
	}
	if !strings.HasPrefix(live, "→ Sync configuration · ") || !strings.Contains(live, "dots up --last") ||
		strings.Contains(replay, "→ Sync configuration") || strings.Contains(replay, "dots up --last") {
		t.Fatalf("unexpected progress or replay hints:\n%s\n%s", live, replay)
	}
	t.Logf("Plain run:\n%sSaved replay:\n%s", live, replay)
}
