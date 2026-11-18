package supervisor

import (
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/creack/pty"
)

func TestScrollbackKeepsChildOutputAndExitCode(t *testing.T) {
	for _, tc := range []struct {
		command, output, diagnostic string
		verbose, sensitive          bool
		code                        int
	}{
		{"remove", "", "dots: no declaration or managed app matches 'codex-bar'\n", false, false, 1},
		{"remove", "", "dots: no declaration or managed app matches 'codex-bar'\n", true, false, 1},
		{"add", "installed fixture-tool\n", "", false, false, 0},
		{"apply", "applied fixture-file\n", "", false, false, 0},
		{"bootstrap", "private fixture output\n", "", false, true, 0},
	} {
		name := tc.command
		if tc.verbose {
			name += "/verbose"
		}
		t.Run(name, func(t *testing.T) {
			useTextRunner(t, runnerEvents+`
[ "$DOTS_UP_VERBOSE" = "$FIXTURE_VERBOSE" ] || exit 5
emit '{"v":1,"kind":"run_start"}'
emit '{"v":1,"kind":"stage_start","id":"'$1'","label":"'$1'"}'
printf '%s' "$FIXTURE_OUTPUT"
printf '%s' "$FIXTURE_DIAGNOSTIC" >&2
emit '{"v":1,"kind":"stage_end","id":"'$1'","status":"'$FIXTURE_STATUS'"}'
exit "$FIXTURE_EXIT"
`)
			t.Setenv("FIXTURE_OUTPUT", tc.output)
			t.Setenv("FIXTURE_DIAGNOSTIC", tc.diagnostic)
			t.Setenv("FIXTURE_VERBOSE", "")
			if tc.verbose {
				t.Setenv("FIXTURE_VERBOSE", "1")
			}
			status := "ok"
			if tc.code != 0 {
				status = "failed"
			}
			t.Setenv("FIXTURE_STATUS", status)
			t.Setenv("FIXTURE_EXIT", strconv.Itoa(tc.code))
			stdout, stderr := captureScrollbackFile(t), captureScrollbackFile(t)
			saveStdout, saveStderr := os.Stdout, os.Stderr
			defer func() { os.Stdout, os.Stderr = saveStdout, saveStderr }()
			os.Stdout, os.Stderr = stdout, stderr
			opts := Options{Command: tc.command, StateDir: t.TempDir(), Verbose: tc.verbose, Sensitive: tc.sensitive}
			code := Run(opts)
			if gotOut, gotErr := readScrollbackFile(t, stdout.Name()), readScrollbackFile(t, stderr.Name()); code != tc.code || gotOut != tc.output || gotErr != tc.diagnostic {
				t.Fatalf("exit %d, stdout %q, stderr %q", code, gotOut, gotErr)
			}
			report, dir, err := latestReport(opts.StateDir)
			if err != nil {
				t.Fatal(err)
			}
			if report.ExitCode != tc.code || report.Command != tc.command {
				t.Fatalf("saved run lost its result: %+v", report)
			}
			if len(report.Events) != 4 {
				t.Fatalf("saved run lost stage events: %+v", report.Events)
			}
			stage := report.Events[2]
			logPath := filepath.Join(dir, "backend.log")
			if tc.sensitive {
				if _, err := os.Stat(logPath); !os.IsNotExist(err) || stage.LogPath != "" {
					t.Fatalf("sensitive command saved a transcript: event = %+v, log error = %v", stage, err)
				}
				logPath = ""
			} else if log := readScrollbackFile(t, logPath); log != tc.output+tc.diagnostic {
				t.Fatalf("saved backend output = %q", log)
			}
			if stage.Status != status || stage.LogPath != logPath {
				t.Fatalf("saved stage = %+v", stage)
			}
		})
	}
}

func TestScrollbackUsesForegroundTerminal(t *testing.T) {
	for _, tc := range []struct {
		name, input string
		code        int
	}{
		{"answer", "q\r", 0},
		{"interrupt", "\x03", 130},
	} {
		t.Run(tc.name, func(t *testing.T) {
			useTextRunner(t, runnerEvents+`
test -t 0 && test -t 1 && test -t 2 || exit 5
emit '{"v":1,"kind":"run_start"}'
emit '{"v":1,"kind":"stage_start","id":"bootstrap","terminal":"handoff"}'
[ -z "$DOTS_UP_ACK_FD" ] || exit 6
printf 'fixture prompt: ' >&2
IFS= read -r answer
[ "$answer" = q ] || exit 7
printf 'fixture complete\n'
emit '{"v":1,"kind":"stage_end","id":"bootstrap","status":"ok"}'
`)
			state := t.TempDir()
			cmd := exec.Command(os.Args[0], "-test.run=^TestScrollbackFixtureProcess$")
			cmd.Env = append(os.Environ(), "DOTS_SCROLLBACK_FIXTURE="+state, "CI=", "TERM=xterm")
			master, err := pty.StartWithSize(cmd, &pty.Winsize{Rows: 30, Cols: 100})
			if err != nil {
				t.Fatal(err)
			}
			defer master.Close()
			defer cmd.Process.Kill()
			output := make(chan string, 1)
			progress := make(chan string, 16)
			go func() {
				var transcript strings.Builder
				buf := make([]byte, 4096)
				var sent bool
				for {
					n, err := master.Read(buf)
					transcript.Write(buf[:n])
					select {
					case progress <- transcript.String():
					default:
					}
					if !sent && strings.Contains(transcript.String(), "fixture prompt: ") {
						sent = true
						_, _ = io.WriteString(master, tc.input)
					}
					if err != nil {
						break
					}
				}
				output <- transcript.String()
			}()
			done := make(chan error, 1)
			go func() { done <- cmd.Wait() }()
			select {
			case err := <-done:
				if code := childExitCode(err); code != tc.code {
					t.Errorf("got exit %d, want %d", code, tc.code)
				}
			case <-time.After(3 * time.Second):
				var last string
				for len(progress) > 0 {
					last = <-progress
				}
				t.Fatalf("the command did not promptly return, terminal output %q", last)
			}
			select {
			case transcript := <-output:
				if !strings.Contains(transcript, "fixture prompt: ") ||
					strings.Contains(transcript, "\x1b[?1049") ||
					(tc.code == 0 && !strings.Contains(transcript, "fixture complete")) {
					t.Fatalf("unexpected terminal output %q", transcript)
				}
			case <-time.After(3 * time.Second):
				t.Fatal("the terminal stayed open")
			}
			report, dir, err := latestReport(state)
			if err != nil || report.ExitCode != tc.code {
				t.Fatalf("saved run exit %d, error %v", report.ExitCode, err)
			}
			if _, err := os.Stat(filepath.Join(dir, "backend.log")); !os.IsNotExist(err) {
				t.Fatalf("sensitive command saved a transcript: %v", err)
			}
		})
	}
}

func TestScrollbackFixtureProcess(t *testing.T) {
	state := os.Getenv("DOTS_SCROLLBACK_FIXTURE")
	if state != "" {
		os.Exit(Run(Options{Command: "bootstrap", StateDir: state, Sensitive: true}))
	}
}

func captureScrollbackFile(t *testing.T) *os.File {
	t.Helper()
	file, err := os.CreateTemp(t.TempDir(), "output")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = file.Close() })
	return file
}

func readScrollbackFile(t *testing.T, name string) string {
	t.Helper()
	data, err := os.ReadFile(name)
	if err != nil {
		t.Fatal(err)
	}
	return string(data)
}
