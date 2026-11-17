package supervisor

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync/atomic"
	"syscall"
	"testing"
	"time"

	"github.com/EzraCerpac/dots/dots-up/internal/ui"
	"github.com/charmbracelet/x/term"
	"github.com/creack/pty"
	"golang.org/x/sys/unix"
)

func TestManualTerminalBackKeysWithRunningPTY(t *testing.T) {
	for _, tc := range []struct {
		name string
		keys []string
		want string
	}{
		{"q", []string{"input", "q"}, "input"},
		{"escape", []string{"\x1b"}, ""},
		{"left", []string{"\x1b[D"}, ""},
		{"fragmented left", []string{"\x1b", "[", "D"}, ""},
		{"other arrows then left", []string{"\x1b", "[", "A\x1b[B\x1b[C\x1bOA", "\x1bOD"}, "\x1b[A\x1b[B\x1b[C\x1bOA"},
		{"modified left", []string{"\x1b[1;2D"}, ""},
		{"legacy control", []string{"\x1d"}, ""},
	} {
		t.Run(tc.name, func(t *testing.T) { runTerminalPTYFixture(t, tc.keys, tc.want, false) })
	}
}

func TestManualTerminalBackAfterTinyResize(t *testing.T) {
	runTerminalPTYFixture(t, []string{"q"}, "", false, terminalFixtureOptions{tinyResize: true})
}

func TestManualTerminalHintPreservesScrollingOutput(t *testing.T) {
	runTerminalPTYFixture(t, []string{"q"}, "", false, terminalFixtureOptions{beforeKeys: 350 * time.Millisecond})
}

func TestManualTerminalHintPreservesScrollingOutputAfterResize(t *testing.T) {
	runTerminalPTYFixture(t, []string{"q"}, "", false, terminalFixtureOptions{beforeKeys: 350 * time.Millisecond, resizeRows: 18})
}

func TestManualTerminalPreservesSplitANSIAndUTF8DuringResize(t *testing.T) {
	runTerminalPTYFixture(t, []string{"q"}, "", false, terminalFixtureOptions{beforeKeys: 350 * time.Millisecond, splitOutput: true})
}

func TestStartupFailureDoesNotPersistTypedTerminalEcho(t *testing.T) {
	const marker = "harmless-typed-echo-fixture\n"
	t.Run("single relay", func(t *testing.T) {
		runTerminalPTYFixture(t, []string{marker}, marker, false, terminalFixtureOptions{startupFailure: true})
	})
	t.Run("relay reopened", func(t *testing.T) {
		runTerminalPTYFixture(t, []string{marker, "q"}, marker, false, terminalFixtureOptions{startupFailure: true, reopenRelay: true})
	})
}

const startupFixtureDiagnostic = "fixture: invalid setup before first stage"

type terminalFixtureOptions struct {
	tinyResize     bool
	splitOutput    bool
	startupFailure bool
	reopenRelay    bool
	resizeRows     int
	beforeKeys     time.Duration
}

func TestAutomaticTerminalHandoffForwardsBackKeys(t *testing.T) {
	const keys = "q\x1b[D\x1b\x1d\x1b[A\x1b[B\x1b[C"
	runTerminalPTYFixture(t, []string{keys}, keys, true)
}

// Use real PTYs and a harmless child that keeps running after dashboard return.
func runTerminalPTYFixture(t *testing.T, keys []string, want string, automatic bool, options ...terminalFixtureOptions) {
	t.Helper()
	var opts terminalFixtureOptions
	if len(options) > 0 {
		opts = options[0]
	}
	oldInteractive, oldSize := hasInteractiveTerminal, currentTerminalSize
	oldStart, oldDashboard := startUpdaterBackend, runDashboard
	t.Cleanup(func() {
		hasInteractiveTerminal, currentTerminalSize = oldInteractive, oldSize
		startUpdaterBackend, runDashboard = oldStart, oldDashboard
	})
	hasInteractiveTerminal = func() bool { return true }
	var terminalHeight atomic.Int32
	terminalHeight.Store(30)
	currentTerminalSize = func() (int, int) { return 100, int(terminalHeight.Load()) }
	terminal, tty, err := pty.Open()
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = terminal.Close(); _ = tty.Close() })
	if err := pty.Setsize(tty, &pty.Winsize{Cols: 100, Rows: 30}); err != nil {
		t.Fatal(err)
	}
	var child *exec.Cmd
	capture := filepath.Join(t.TempDir(), "input")
	fixtureReady := filepath.Join(t.TempDir(), "ready")
	startUpdaterBackend = func(Options, *runFiles, bool) (*backend, *os.File, io.Reader, error) {
		eventR, eventW, err := os.Pipe()
		if err != nil {
			return nil, nil, nil, err
		}
		child = exec.Command(os.Args[0], "-test.run=^TestTerminalPTYFixtureProcess$")
		child.Env = append(os.Environ(), "DOTS_TERMINAL_FIXTURE="+capture)
		if automatic {
			child.Env = append(child.Env, "DOTS_TERMINAL_FIXTURE_AUTO=1")
		}
		if opts.splitOutput {
			child.Env = append(child.Env, "DOTS_TERMINAL_FIXTURE_SPLIT="+fixtureReady)
		}
		var nonce string
		if opts.startupFailure {
			nonce = "0123456789abcdef0123456789abcdef"
			child.Env = append(child.Env, "DOTS_TERMINAL_FIXTURE_STARTUP="+nonce)
		}
		if opts.reopenRelay {
			child.Env = append(child.Env, "DOTS_TERMINAL_FIXTURE_REOPEN="+fixtureReady)
		}
		child.ExtraFiles = []*os.File{eventW}
		master, err := pty.StartWithSize(child, &pty.Winsize{Cols: 100, Rows: 30})
		_ = eventW.Close()
		if err != nil {
			_ = eventR.Close()
			return nil, nil, nil, err
		}
		return &backend{command: child, master: master, tty: tty, pty: true, processGroup: true, streamNonce: nonce}, eventR, master, nil
	}
	ready := make(chan struct{})
	reopened := make(chan struct{})
	readDone := make(chan string, 1)
	go func() {
		var output strings.Builder
		buf := make([]byte, 4096)
		signaled := false
		reopenSignaled := false
		resizedANSI, resizedUTF8 := false, false
		for {
			n, err := terminal.Read(buf)
			if n > 0 {
				output.Write(buf[:n])
				if opts.splitOutput {
					rows := 0
					if !resizedANSI && strings.Contains(output.String(), "\x1b[31") {
						rows, resizedANSI = 18, true
					} else if !resizedUTF8 && strings.Contains(output.String(), "\xe2\x82") {
						rows, resizedUTF8 = 20, true
					}
					if rows > 0 {
						terminalHeight.Store(int32(rows))
						_ = pty.Setsize(tty, &pty.Winsize{Cols: 100, Rows: uint16(rows)})
						_ = syscall.Kill(os.Getpid(), syscall.SIGWINCH)
					}
				}
				marker := "dots up keeps running"
				if automatic {
					marker = "fixture output"
				}
				if !signaled && strings.Contains(output.String(), marker) {
					close(ready)
					signaled = true
				}
				if opts.reopenRelay && !reopenSignaled && strings.Count(output.String(), marker) >= 2 {
					close(reopened)
					reopenSignaled = true
				}
			}
			if err != nil {
				readDone <- output.String()
				return
			}
		}
	}()
	returned := false
	runDashboard = func(events <-chan ui.Event, control io.Writer, _ string, _ bool) error {
		encoder := json.NewEncoder(control)
		for event := range events {
			switch event.Kind {
			case "stage_start", "run_start":
				if (event.Kind == "run_start") != opts.startupFailure {
					continue
				}
				// Reproduce returning to a primary screen whose cursor is at the bottom.
				fmt.Fprint(tty, "\x1b[30;1H")
				kind := "terminal_ready"
				if automatic {
					kind = "handoff_ready"
				}
				if err := encoder.Encode(controlRecord{V: 1, Kind: kind, ID: event.ID}); err != nil {
					return err
				}
				select {
				case <-ready:
				case <-time.After(3 * time.Second):
					return fmt.Errorf("terminal relay did not become ready")
				}
				if opts.tinyResize || opts.resizeRows > 0 {
					rows := opts.resizeRows
					if opts.tinyResize {
						rows = 1
					}
					terminalHeight.Store(int32(rows))
					if err := pty.Setsize(tty, &pty.Winsize{Cols: 100, Rows: uint16(rows)}); err != nil {
						return err
					}
					if err := syscall.Kill(os.Getpid(), syscall.SIGWINCH); err != nil {
						return err
					}
					time.Sleep(20 * time.Millisecond)
				}
				if opts.splitOutput {
					if err := os.WriteFile(fixtureReady, nil, 0o600); err != nil {
						return err
					}
				}
				time.Sleep(opts.beforeKeys)
				for _, key := range keys {
					if _, err := io.WriteString(terminal, key); err != nil {
						return err
					}
					time.Sleep(20 * time.Millisecond)
				}
			case "terminal_end":
				if automatic {
					return fmt.Errorf("back keys ended automatic handoff")
				}
				if !opts.startupFailure {
					if err := child.Process.Signal(syscall.Signal(0)); err != nil {
						return fmt.Errorf("back key stopped updater: %w", err)
					}
				}
				if err := encoder.Encode(controlRecord{V: 1, Kind: "terminal_ack"}); err != nil {
					return err
				}
				if opts.reopenRelay && !returned {
					if err := encoder.Encode(controlRecord{V: 1, Kind: "terminal_ready"}); err != nil {
						return err
					}
					select {
					case <-reopened:
					case <-time.After(3 * time.Second):
						return fmt.Errorf("terminal relay did not reopen")
					}
					if err := os.WriteFile(fixtureReady, nil, 0o600); err != nil {
						return err
					}
				}
				returned = true
			case "handoff_end":
				returned = true
				if err := encoder.Encode(controlRecord{V: 1, Kind: "handoff_ack", ID: event.ID}); err != nil {
					return err
				}
			}
		}
		return nil
	}
	state := filepath.Join(t.TempDir(), "state")
	runOpts := Options{Root: t.TempDir(), StateDir: state}
	wantCode := 0
	var code int
	var reportText string
	if opts.startupFailure {
		wantCode = 2
		code, reportText = captureRun(t, runOpts)
	} else {
		code = Run(runOpts)
	}
	if code != wantCode {
		t.Fatalf("run exit code = %d, want %d", code, wantCode)
	}
	if !returned {
		t.Fatal("dashboard was not restored")
	}
	input, err := os.ReadFile(capture)
	if err != nil {
		t.Fatal(err)
	}
	if string(input) != want {
		t.Fatalf("child received %q, want %q", input, want)
	}
	report, reportDir, err := latestReport(state)
	if err != nil || report.ExitCode != wantCode {
		t.Fatalf("update did not finish normally: report=%+v, err=%v", report, err)
	}
	_ = tty.Close()
	output := <-readDone
	if opts.startupFailure {
		marker := strings.TrimSpace(want)
		if !strings.Contains(output, marker) {
			t.Fatalf("backend PTY never echoed the harmless input: %q", output)
		}
		if !strings.Contains(reportText, startupFixtureDiagnostic) || strings.Contains(reportText, marker) {
			t.Errorf("unexpected console startup diagnostic: %q", reportText)
		}
		for _, name := range []string{"backend.log", "events.jsonl", "report.json"} {
			data, err := os.ReadFile(filepath.Join(reportDir, name))
			if err != nil || !strings.Contains(string(data), startupFixtureDiagnostic) || strings.Contains(string(data), marker) {
				t.Errorf("unexpected diagnostic in %s: %q (%v)", name, data, err)
			}
		}
	}
	if capture := os.Getenv("DOTS_TERMINAL_SCREEN_CAPTURE"); capture != "" {
		if err := os.WriteFile(capture, []byte(output), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	if opts.splitOutput {
		if !strings.Contains(output, "\x1b[31mcolored") || !strings.Contains(output, "€") {
			t.Fatalf("manual hints interrupted split child output: %q", output)
		}
		if !strings.Contains(output, "\x1b[1;19r") {
			t.Fatalf("pending manual resize was not applied: %q", output)
		}
	}
	if !automatic {
		if !strings.Contains(output, "\x1b[1;29r") || !strings.Contains(output, "\x1b[r") {
			t.Fatalf("manual hint scrolling region was not installed and reset: %q", output)
		}
		if opts.tinyResize && !strings.Contains(output, "\x1b7\x1b[r\x1b8") {
			t.Fatal("tiny resize did not reset the manual scrolling region")
		}
	} else if strings.Contains(output, "keeps running") {
		t.Fatal("manual hint was displayed during automatic handoff")
	}
}

func TestTerminalPTYFixtureProcess(t *testing.T) {
	capture := os.Getenv("DOTS_TERMINAL_FIXTURE")
	if capture == "" {
		return
	}
	if nonce := os.Getenv("DOTS_TERMINAL_FIXTURE_STARTUP"); nonce != "" {
		reopen := os.Getenv("DOTS_TERMINAL_FIXTURE_REOPEN")
		if reopen != "" {
			// Delay the echo of the first relay's input until the second relay is active.
			if _, err := term.MakeRaw(os.Stdin.Fd()); err != nil {
				os.Exit(3)
			}
		}
		fmt.Println(startupFixtureDiagnostic)
		fmt.Printf("%s%s:%s%c", streamEventStart, nonce, `{"v":1,"kind":"run_start"}`, streamEventEnd)
		input, err := bufio.NewReader(os.Stdin).ReadString('\n')
		if err != nil || os.WriteFile(capture, []byte(input), 0o600) != nil {
			os.Exit(3)
		}
		if reopen != "" {
			deadline := time.Now().Add(4 * time.Second)
			for {
				if _, err := os.Stat(reopen); err == nil {
					break
				}
				if time.Now().After(deadline) {
					os.Exit(3)
				}
				time.Sleep(10 * time.Millisecond)
			}
			fmt.Print(input)
		}
		os.Exit(2)
	}
	if _, err := term.MakeRaw(os.Stdin.Fd()); err != nil {
		os.Exit(2)
	}
	events := json.NewEncoder(os.NewFile(3, "events"))
	_ = events.Encode(ui.Event{V: 1, Kind: "run_start", Profile: "fixture", Host: "fixture"})
	stage := ui.Event{V: 1, Kind: "stage_start", ID: "fixture", Label: "PTY fixture"}
	if os.Getenv("DOTS_TERMINAL_FIXTURE_AUTO") != "" {
		stage.Terminal = "handoff"
	}
	_ = events.Encode(stage)
	if ready := os.Getenv("DOTS_TERMINAL_FIXTURE_SPLIT"); ready != "" {
		for {
			if _, err := os.Stat(ready); err == nil {
				break
			}
			time.Sleep(time.Millisecond)
		}
		fmt.Print("\x1b[31")
		time.Sleep(80 * time.Millisecond)
		fmt.Print("mcolored\x1b[0m\r\n\xe2\x82")
		time.Sleep(80 * time.Millisecond)
		fmt.Print("\xac\r\n")
	}
	var input bytes.Buffer
	until := time.Now().Add(500 * time.Millisecond)
	buf := make([]byte, 256)
	for time.Now().Before(until) {
		fmt.Print("fixture output first\r\nfixture output second\r\n")
		var readable unix.FdSet
		readable.Set(0)
		timeout := unix.NsecToTimeval(int64(20 * time.Millisecond))
		if n, err := unix.Select(1, &readable, nil, nil, &timeout); err == nil && n > 0 {
			if n, err := unix.Read(0, buf); err == nil {
				input.Write(buf[:n])
			}
		}
	}
	if err := os.WriteFile(capture, input.Bytes(), 0o600); err != nil {
		os.Exit(3)
	}
	_ = events.Encode(ui.Event{V: 1, Kind: "stage_end", ID: "fixture", Status: "ok"})
	_ = events.Encode(ui.Event{V: 1, Kind: "run_end", ExitCode: 0})
	os.Exit(0)
}
