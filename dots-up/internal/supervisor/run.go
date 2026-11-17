package supervisor

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/EzraCerpac/dots/dots-up/internal/ui"
	"github.com/charmbracelet/x/ansi"
	"github.com/charmbracelet/x/ansi/parser"
	"github.com/charmbracelet/x/term"
	"github.com/creack/pty"
	"golang.org/x/sys/unix"
)

type Options struct {
	// Root is the configuration (DOTS_ROOT); Home is the manager (DOTS_HOME)
	// whose scripts run it. dots passes both explicitly.
	Root, Home, StateDir        string
	Command                     string
	Args                        []string
	Sensitive                   bool
	Concise                     bool
	MAS, Plain, Verbose, NoWait bool
	Last                        bool
}

type controlRecord struct {
	V    int    `json:"v"`
	Kind string `json:"kind"`
	ID   string `json:"id,omitempty"`
}

type backend struct {
	command        *exec.Cmd
	master         *os.File
	tty            *os.File
	ack            *os.File
	streamNonce    string
	pty            bool
	processGroup   bool
	directTerminal bool
}

type relayState struct {
	active        atomic.Bool
	manual        atomic.Bool
	inputSent     atomic.Bool
	inputEverSent atomic.Bool
	inputMu       sync.Mutex
}

var (
	hasInteractiveTerminal = interactiveTerminal
	currentTerminalSize    = terminalSize
	startUpdaterBackend    = startBackend
	runDashboard           = ui.Run
)

const startupStage = "startup"

func startupFailure(command string, code int, detail string, log *os.File) (ui.Event, ui.Event) {
	start := ui.Event{V: 1, Kind: "stage_start", ID: startupStage, Label: "Startup"}
	if log != nil {
		start.LogPath = log.Name()
	}
	end := start
	end.Kind, end.Status, end.ExitCode = "stage_end", "failed", code
	end.Summary, end.Detail = fmt.Sprintf("exit %d", code), detail
	end.Action = fmt.Sprintf("Fix the startup error, then rerun dots %s.", command)
	return start, end
}

func printStartupFailure(w io.Writer, e ui.Event) {
	printIssue(w, fmt.Sprintf("Failed: %s — %s", e.Label, e.Summary), e.Detail, e.Action, e.LogPath)
}

func Run(opts Options) int {
	if opts.Command == "" {
		opts.Command = "up"
	}
	if opts.StateDir == "" {
		opts.StateDir = commandStateDir(opts.Command)
	}
	started := time.Now()
	files, err := newRunFiles(started, opts.StateDir)
	if err != nil {
		fmt.Fprintf(os.Stderr, "dots up: reports unavailable: %v\nContinuing without saved logs.\n", err)
	}
	if files != nil {
		files.report.Command = opts.Command
		defer func() {
			if files.eventLog != nil {
				_ = files.eventLog.Close()
			}
		}()
	}

	stateRoot := stateDir()
	if opts.StateDir != "" {
		stateRoot = opts.StateDir
	}
	interactive := hasInteractiveTerminal()
	dashboard := interactive && !opts.Plain && !opts.Verbose && !opts.Concise
	if dashboard {
		width, height := currentTerminalSize()
		if width < 64 || height < 16 {
			dashboard = false
		}
	}
	plain := !dashboard
	if opts.Verbose {
		plain = true
	}
	if interactive && !dashboard && !opts.Verbose && !opts.Plain && !opts.Concise {
		fmt.Fprintln(os.Stderr, "dots up: terminal is too small for the dashboard; using concise output")
	}

	interrupts := make(chan os.Signal, 1)
	signal.Notify(interrupts, os.Interrupt, syscall.SIGTERM)
	defer signal.Stop(interrupts)
	resizes := make(chan os.Signal, 1)
	signal.Notify(resizes, syscall.SIGWINCH)
	defer signal.Stop(resizes)

	var globalLog *os.File
	if !opts.Sensitive {
		globalLog = openLog(files, "backend.log")
		if globalLog != nil {
			defer globalLog.Close()
		}
	}
	backend, eventReader, outputReader, err := startUpdaterBackend(opts, files, interactive)
	if err != nil {
		const code = 127
		detail := string(cleanTerminalOutput([]byte(err.Error())))
		writeLog(nil, globalLog, []byte(detail+"\n"))
		start, issue := startupFailure(opts.Command, code, detail, globalLog)
		if files != nil {
			_ = files.record(ui.Event{V: 1, Kind: "run_start", Command: opts.Command})
			_ = files.record(start)
			_ = files.record(issue)
			_ = files.record(ui.Event{V: 1, Kind: "run_end", Status: "failed", ExitCode: code, DurationMS: time.Since(started).Milliseconds()})
			_ = files.finish(code, "", "", time.Now())
		}
		printStartupFailure(os.Stderr, issue)
		return code
	}
	if dashboard && !backend.pty {
		dashboard, plain = false, true
		fmt.Fprintln(os.Stderr, "dots up: pseudoterminal unavailable; using concise terminal output")
	}
	var originalTerminalState *term.State
	if backend.tty != nil {
		originalTerminalState, _ = term.GetState(backend.tty.Fd())
	}
	var events chan ui.Event
	var uiDone chan error
	var controlReader *io.PipeReader
	var controlWriter *io.PipeWriter
	controls := make(chan controlRecord, 16)
	if dashboard {
		events = make(chan ui.Event, 512)
		controlReader, controlWriter = io.Pipe()
		uiDone = make(chan error, 1)
		go func() {
			err := runDashboard(events, controlWriter, stateRoot, opts.NoWait)
			_ = controlWriter.Close()
			uiDone <- err
		}()
		go readControls(controlReader, controls)
	}
	dashboardStarted := dashboard
	defer func() {
		if originalTerminalState != nil && backend.tty != nil {
			_ = term.Restore(backend.tty.Fd(), originalTerminalState)
		}
		if backend.master != nil {
			_ = backend.master.Close()
		}
		if backend.tty != nil {
			_ = backend.tty.Close()
		}
		if backend.ack != nil {
			_ = backend.ack.Close()
		}
	}()

	backendEvents := make(chan ui.Event, 32)
	eventErr := make(chan error, 1)
	if backend.streamNonce == "" {
		go readEvents(eventReader, backendEvents, eventErr)
	} else {
		_ = eventReader.Close()
		close(backendEvents)
	}
	backendOutput := make(chan outputRecord, 32)
	if outputReader == nil {
		close(backendOutput)
	} else {
		go readOutput(outputReader, backend.streamNonce, backendOutput)
	}
	waitCh := make(chan error, 1)
	go func() { waitCh <- backend.command.Wait() }()

	var stageLog *os.File
	var stageID, profile, host string
	var runStarted, workReported, childFinished, outputFinished bool
	var startupOutput []byte
	runEnd := ui.Event{V: 1, Kind: "run_end"}
	eventFinished := backend.streamNonce != ""
	var exitCode int
	var waitErr error
	var backendError error
	var pendingHandoff string
	var relayKind string
	var terminalState *term.State
	var manualHintRows, manualHintCols int
	manualOutputParser := ansi.NewParser()
	var manualResizePending bool
	var deferredStageEnd *ui.Event
	var restoreKind, restoreID string
	var restorePending bool
	var deferredUIEvents []ui.Event
	var cancelRequested bool
	var uiAlive = dashboard

	var eventInput <-chan ui.Event
	if backend.streamNonce == "" {
		eventInput = backendEvents
	}
	var outputInput <-chan outputRecord = backendOutput
	var uiInput <-chan error = uiDone
	var uiExited bool
	var uiErr error
	manualEnd := make(chan struct{}, 1)
	var inputStart chan struct{}
	var relay relayState
	if backend.pty && backend.tty != nil && backend.master != nil {
		inputStart = make(chan struct{}, 1)
		defer close(inputStart)
		go relayTerminalInput(backend.tty, backend.master, &relay, inputStart, manualEnd)
	}

	defer func() {
		if stageLog != nil {
			_ = stageLog.Close()
		}
		if controlReader != nil {
			_ = controlReader.Close()
		}
	}()

	labels := map[string]string{} // a stage_end may name its stage only by ID
	var dashboardStatus []ui.Event
	flushDashboardStatus := func() {
		for _, event := range dashboardStatus {
			printStatusEvent(event)
		}
		dashboardStatus = nil
	}
	deliver := func(e ui.Event) {
		if e.Kind == "stage_start" {
			labels[e.ID] = e.Label
		} else if e.Kind == "stage_end" && e.Label == "" {
			e.Label = labels[e.ID]
		}
		if uiAlive && events != nil {
			select {
			case events <- e:
			default:
				uiAlive, dashboard, plain = false, false, true
				fmt.Fprintln(os.Stderr, "dots up: dashboard stopped responding; continuing in concise mode")
			}
		}
		if !plain {
			if opts.Command == "status" && (e.Kind == "run_start" || e.Kind == "item_upsert") {
				dashboardStatus = append(dashboardStatus, e)
			}
			return
		}
		// On fallback, print the status rows that only the dashboard saw.
		flushDashboardStatus()
		if opts.Command == "status" && printStatusEvent(e) {
			return
		}
		if !opts.Verbose && e.ID != startupStage {
			printProgress(e)
		}
	}
	emit := func(e ui.Event) {
		if e.V == 0 {
			e.V = 1
		}
		if e.Kind == "run_start" {
			e.Command = opts.Command
		}
		if opts.Sensitive {
			e.LogPath = ""
		}
		if files != nil {
			if err := files.record(e); err != nil {
				fmt.Fprintf(os.Stderr, "dots up: report log: %v\n", err)
			}
		}
		if restorePending && e.Kind != "handoff_end" && e.Kind != "terminal_end" {
			deferredUIEvents = append(deferredUIEvents, e)
			return
		}
		deliver(e)
	}
	redrawManualHint := func() {
		width, height := currentTerminalSize()
		initialize := manualHintRows != height || manualHintCols != width
		if initialize && manualHintRows > 0 && manualHintRows <= height {
			// After growth, the previous footer is now an ordinary content row.
			fmt.Fprintf(backend.tty, "\x1b[%d;1H\x1b[2K", manualHintRows)
		}
		manualHintRows = drawManualTerminalHint(backend.tty, width, height, initialize, opts.Command)
		manualHintCols = width
		if manualHintRows == 0 {
			fmt.Fprint(backend.tty, "\x1b7\x1b[r\x1b8")
		}
	}
	startRelay := func(kind string) error {
		if !backend.pty || backend.tty == nil {
			return errors.New("terminal handoff needs an interactive pseudoterminal")
		}
		state, err := term.MakeRaw(backend.tty.Fd())
		if err != nil {
			return fmt.Errorf("enable terminal input: %w", err)
		}
		terminalState = state
		relay.manual.Store(kind == "manual")
		relay.inputSent.Store(false)
		relay.active.Store(true)
		select {
		case inputStart <- struct{}{}:
		default:
		}
		relayKind = kind
		if kind == "manual" {
			manualOutputParser.Reset()
			manualResizePending = false
			redrawManualHint()
		}
		return nil
	}
	stopRelay := func() error {
		relay.active.Store(false)
		relay.inputMu.Lock()
		relay.inputMu.Unlock()
		if relay.manual.Load() && backend.tty != nil {
			if manualHintRows > 0 {
				fmt.Fprintf(backend.tty, "\x1b7\x1b[r\x1b[%d;1H\x1b[2K\x1b8", manualHintRows)
			} else {
				fmt.Fprint(backend.tty, "\x1b7\x1b[r\x1b8")
			}
			manualHintRows, manualHintCols = 0, 0
		}
		if terminalState == nil || backend.tty == nil {
			return nil
		}
		err := term.Restore(backend.tty.Fd(), terminalState)
		terminalState = nil
		return err
	}
	ackChild := func(id string) error {
		if backend.ack == nil {
			return nil
		}
		if _, err := io.WriteString(backend.ack, "go:"+id+"\n"); err != nil {
			return fmt.Errorf("acknowledge terminal handoff: %w", err)
		}
		return nil
	}
	startAutomaticRelay := func() {
		err := startRelay("auto")
		if err == nil {
			err = ackChild(pendingHandoff)
		}
		if err != nil {
			backendError = err
			cancelRequested = true
			cancelBackend(backend)
		}
	}
	finishStage := func(e ui.Event) {
		if stageLog != nil && stageID == e.ID {
			_ = stageLog.Sync()
			_ = stageLog.Close()
			stageLog = nil
		}
		if e.LogPath == "" && files != nil {
			e.LogPath = files.logPath(e.ID)
		}
		emit(e)
		stageID = ""
	}
	beginRestore := func(kind, id string, endEvent ui.Event, finishStageAfter bool) {
		if endEvent.Kind != "" {
			deferredStageEnd = &endEvent
		}
		restoreKind, restoreID = kind, id
		restorePending = true
		eventInput = nil
		if backend.streamNonce != "" {
			outputInput = nil
		}
		if kind == "auto" {
			emit(ui.Event{V: 1, Kind: "handoff_end", ID: id})
		} else {
			emit(ui.Event{V: 1, Kind: "terminal_end"})
		}
		if !finishStageAfter {
			deferredStageEnd = nil
		}
	}
	finishRestore := func() {
		if deferredStageEnd != nil {
			finishStage(*deferredStageEnd)
			deferredStageEnd = nil
		}
		restoreKind, restoreID, restorePending = "", "", false
		if backend.streamNonce == "" {
			eventInput = backendEvents
		} else if !outputFinished {
			outputInput = backendOutput
		}
		for _, e := range deferredUIEvents {
			deliver(e)
		}
		deferredUIEvents = nil
	}
	handleEvent := func(e ui.Event) {
		if e.V != 1 {
			backendError = fmt.Errorf("unsupported updater event version %d", e.V)
			return
		}
		switch e.Kind {
		case "run_start":
			runStarted = true
			profile, host = e.Profile, e.Host
			emit(e)
		case "stage_start":
			workReported = true
			if stageLog != nil {
				_ = stageLog.Close()
				stageLog = nil
			}
			stageID = e.ID
			if files != nil && !opts.Sensitive {
				e.LogPath = files.logPath(e.ID)
				stageLog = openStageLog(e.LogPath)
			}
			if e.Terminal == "handoff" && backend.pty {
				// Automatic prompts take ownership from a manual view. Stop its
				// input filtering and clear its footer before the UI prints the prompt.
				if relayKind == "manual" {
					if err := stopRelay(); err != nil {
						backendError = err
					}
					relayKind = ""
				}
				pendingHandoff = e.ID
			}
			emit(e)
			if e.Terminal == "handoff" && backend.pty {
				if !dashboard {
					startAutomaticRelay()
				}
			}
		case "stage_end":
			workReported = true
			if e.LogPath == "" && files != nil {
				e.LogPath = files.logPath(e.ID)
			}
			if pendingHandoff == e.ID {
				if relayKind == "auto" {
					if err := stopRelay(); err != nil {
						backendError = err
					}
				}
				pendingHandoff, relayKind = "", ""
				if dashboard {
					beginRestore("auto", e.ID, e, true)
					return
				}
			}
			if relayKind == "manual" {
				if err := stopRelay(); err != nil {
					backendError = err
				}
				relayKind, pendingHandoff = "", ""
				beginRestore("manual", "manual", e, true)
				return
			}
			finishStage(e)
		case "run_end":
			// The child can report its outcome before its output has drained.
			runEnd, exitCode = e, e.ExitCode
		default:
			if e.Kind == "notice" && (e.Severity == "attention" || e.Severity == "error") {
				workReported = true
			}
			if (e.Kind == "app_store_start" || e.Kind == "app_store_end") && e.LogPath != "" && files != nil && !inside(files.runDir, e.LogPath) {
				e.LogPath = ""
			}
			emit(e)
		}
	}
	finishExitedHandoff := func() {
		if !childFinished || !outputFinished || pendingHandoff == "" {
			return
		}
		id := pendingHandoff
		if relayKind != "" {
			if err := stopRelay(); err != nil {
				backendError = err
			}
		}
		pendingHandoff, relayKind = "", ""
		if id == "manual" {
			if dashboard {
				beginRestore("manual", "manual", ui.Event{}, false)
			}
			return
		}
		status := "attention"
		summaryText := "Updater exited before this step reported a result."
		if exitCode != 0 {
			status = "failed"
			summaryText = fmt.Sprintf("Updater exited with code %d before this step reported a result.", exitCode)
		}
		end := ui.Event{V: 1, Kind: "stage_end", ID: id, Status: status, ExitCode: exitCode, Summary: summaryText, Action: "Review the stage log and rerun dots up."}
		if dashboard {
			beginRestore("auto", id, end, true)
		} else {
			finishStage(end)
		}
	}

	for !(childFinished && eventFinished && outputFinished && !restorePending) {
		select {
		case e, ok := <-eventInput:
			if !ok {
				eventFinished = true
				eventInput = nil
				continue
			}
			handleEvent(e)
		case control := <-controls:
			switch control.Kind {
			case "cancel":
				if !cancelRequested && !childFinished {
					cancelRequested = true
					cancelBackend(backend)
				}
			case "handoff_ready":
				if dashboard && pendingHandoff == control.ID && relayKind == "" {
					startAutomaticRelay()
				}
			case "handoff_ack":
				if restorePending && restoreKind == "auto" && restoreID == control.ID {
					finishRestore()
				}
			case "terminal_request":
				// The UI releases the terminal before sending terminal_ready.
			case "terminal_ready":
				// A manual release may race with the next automatic prompt. Its
				// stale readiness must not replace that prompt's pending ACK.
				if dashboard && backend.pty && relayKind == "" && pendingHandoff == "" && !restorePending {
					if err := startRelay("manual"); err != nil {
						backendError = err
					} else {
						pendingHandoff = "manual"
					}
				}
			case "terminal_ack":
				if restorePending && restoreKind == "manual" {
					finishRestore()
				}
			}
		case item, ok := <-outputInput:
			if !ok {
				outputFinished = true
				outputInput = nil
				finishExitedHandoff()
				continue
			}
			if item.err != nil {
				backendError = item.err
			}
			if item.event != nil {
				handleEvent(*item.event)
				continue
			}
			b := item.bytes
			relaying := relayKind != "" && backend.tty != nil
			if relaying {
				_, _ = backend.tty.Write(b)
				if relayKind == "manual" {
					manualOutputParser.Parse(b)
					if manualResizePending && manualOutputParser.State() == parser.GroundState {
						redrawManualHint()
						manualResizePending = false
					}
				}
			}
			inputMayEcho := relaying && relay.inputSent.Load()
			if !workReported {
				// A reopened relay can still drain echo from an earlier one.
				inputMayEcho = relay.inputEverSent.Load()
			}
			if !inputMayEcho {
				clean := cleanTerminalOutput(b)
				if !workReported && !opts.Sensitive {
					const limit = 8 * 1024
					startupOutput = append(startupOutput, clean...)
					if len(startupOutput) > limit {
						startupOutput = append(startupOutput[:0], startupOutput[len(startupOutput)-limit:]...)
					}
				}
				writeLog(stageLog, globalLog, clean)
			}
			if !relaying && opts.Verbose && len(b) > 0 {
				_, _ = os.Stdout.Write(b)
			}
		case err := <-eventErr:
			if err != nil {
				backendError = err
			}
		case err := <-waitCh:
			waitCh = nil
			childFinished, waitErr = true, err
			if err != nil {
				exitCode = childExitCode(err)
			}
			if cancelRequested {
				exitCode = 130
				if opts.Command == "remote" {
					emit(ui.Event{V: 1, Kind: "notice", Severity: "attention", Summary: "Remote execution status is unknown after local cancellation; inspect the host before retrying.", Action: "Check the remote host before retrying; local cancellation does not confirm remote termination."})
				}
			}
			if relayKind != "" {
				_ = stopRelay()
			}
			if backend.ack != nil {
				_ = backend.ack.Close()
				backend.ack = nil
			}
			finishExitedHandoff()
		case err := <-uiInput:
			uiInput = nil
			uiExited, uiErr = true, err
			if err != nil || !childFinished {
				if dashboard {
					fmt.Fprintf(os.Stderr, "dots up: dashboard unavailable (%v); continuing in concise mode\n", err)
				}
				uiAlive, dashboard, plain = false, false, true
				if restorePending {
					finishRestore()
				}
				if relayKind == "manual" {
					_ = stopRelay()
					relayKind = ""
				}
				if pendingHandoff == "manual" {
					pendingHandoff = ""
				}
				if pendingHandoff != "" && relayKind == "" && backend.pty {
					startAutomaticRelay()
				}
			}
		case <-resizes:
			if !childFinished && backend.pty && backend.tty != nil && backend.master != nil {
				_ = pty.InheritSize(backend.tty, backend.master)
				if relayKind == "manual" {
					// Reads can end inside ANSI controls or a UTF-8 rune. Keep the
					// child stream intact and wait for a complete sequence to resize.
					manualResizePending = manualOutputParser.State() != parser.GroundState
					if !manualResizePending {
						redrawManualHint()
					}
				}
			}
		case <-interrupts:
			if !cancelRequested && !childFinished {
				cancelRequested = true
				cancelBackend(backend)
			}
		case <-manualEnd:
			if relayKind == "manual" && dashboard {
				if err := stopRelay(); err != nil {
					backendError = err
				}
				relayKind, pendingHandoff = "", ""
				beginRestore("manual", "manual", ui.Event{}, false)
			}
		}
	}

	if terminalState != nil {
		_ = stopRelay()
	}
	if stageLog != nil {
		_ = stageLog.Sync()
		_ = stageLog.Close()
	}
	if cancelRequested {
		exitCode = 130
	} else if waitErr != nil && exitCode == 0 {
		exitCode = childExitCode(waitErr)
	}
	if backendError != nil && exitCode == 0 {
		exitCode = 1
	}
	var startupIssue *ui.Event
	if exitCode != 0 && !cancelRequested && !workReported {
		if !runStarted && opts.Command != "up" {
			emit(ui.Event{V: 1, Kind: "run_start"})
		}
		start, issue := startupFailure(opts.Command, exitCode, string(startupOutput), globalLog)
		startupIssue = &issue
		emit(start)
		emit(issue)
		runEnd.Status = "failed"
	}
	runEnd.ExitCode = exitCode
	if runEnd.DurationMS == 0 {
		runEnd.DurationMS = time.Since(started).Milliseconds()
	}
	emit(runEnd)
	if files != nil {
		_ = files.finish(exitCode, profile, host, time.Now())
	}
	if events != nil {
		close(events)
	}
	if uiDone != nil && !uiExited {
		uiErr = <-uiDone
		uiExited = true
	}
	if uiErr != nil {
		fmt.Fprintf(os.Stderr, "dots up: dashboard ended with an error: %v\n", uiErr)
		plain = true
		flushDashboardStatus()
	}
	if (plain && !opts.Verbose) || dashboardStarted {
		if files != nil {
			fmt.Println(summary(files.report, files.runDir))
			printReportHighlights(os.Stdout, files.report, files.runDir, plain)
		} else {
			fmt.Printf("Update completed with exit code %d.\n", exitCode)
		}
	}
	if startupIssue != nil && (files == nil || opts.Verbose) {
		if opts.Verbose {
			startupIssue.Detail = "" // verbose output already showed the diagnostic
		}
		printStartupFailure(os.Stdout, *startupIssue)
	}
	if backendError != nil {
		fmt.Fprintf(os.Stderr, "dots up: supervisor warning: %v\n", backendError)
	}
	return exitCode
}

func startBackend(opts Options, files *runFiles, interactive bool) (*backend, *os.File, io.Reader, error) {
	cmd, err := updaterCommand(opts)
	if err != nil {
		return nil, nil, nil, err
	}
	eventR, eventW, err := os.Pipe()
	if err != nil {
		return nil, nil, nil, err
	}
	ackR, ackW, err := os.Pipe()
	if err != nil {
		_ = eventR.Close()
		_ = eventW.Close()
		return nil, nil, nil, err
	}
	if opts.MAS {
		cmd.Args = append(cmd.Args, "--mas")
	}
	childEnv := append([]string(nil), os.Environ()...)
	childEnv = setEnv(childEnv, "DOTS_ROOT", opts.Root)
	childEnv = setEnv(childEnv, "DOTS_HOME", opts.Home)
	childEnv = setEnv(childEnv, "DOTS_UP_EVENT_FD", "3")
	childEnv = setEnv(childEnv, "DOTS_UP_ACK_FD", "")
	childEnv = setEnv(childEnv, "DOTS_UP_RUN_STARTED", "")
	childEnv = setEnv(childEnv, "DOTS_UP_RUN_DIR", "")
	childEnv = setEnv(childEnv, "DOTS_UP_PLAIN", "")
	childEnv = setEnv(childEnv, "DOTS_UP_VERBOSE", "")
	if opts.Plain || !interactive {
		childEnv = setEnv(childEnv, "DOTS_UP_PLAIN", "1")
	}
	if opts.Verbose {
		childEnv = setEnv(childEnv, "DOTS_UP_VERBOSE", "1")
	}
	if files != nil {
		childEnv = setEnv(childEnv, "DOTS_UP_RUN_DIR", files.runDir)
	}

	var tty *os.File
	ptyMode := interactive
	if ptyMode {
		tty, err = os.OpenFile("/dev/tty", os.O_RDWR, 0)
		if err != nil || !term.IsTerminal(tty.Fd()) {
			ptyMode = false
			if tty != nil {
				_ = tty.Close()
				tty = nil
			}
		}
	}
	streamNonce := ""
	if ptyMode {
		streamNonce, err = newStreamNonce()
		if err != nil {
			_ = eventR.Close()
			_ = eventW.Close()
			_ = ackR.Close()
			_ = ackW.Close()
			if tty != nil {
				_ = tty.Close()
			}
			return nil, nil, nil, err
		}
		childEnv = setEnv(childEnv, "DOTS_UP_STREAM_NONCE", streamNonce)
		childEnv = setEnv(childEnv, "DOTS_UP_EVENT_FD", "")
	}
	cmd.ExtraFiles = []*os.File{eventW}
	if ptyMode {
		childEnv = setEnv(childEnv, "DOTS_UP_ACK_FD", "4")
		cmd.ExtraFiles = []*os.File{eventW, ackR}
		cmd.Env = childEnv
		width, height := terminalSize()
		master, startErr := pty.StartWithSize(cmd, &pty.Winsize{Cols: uint16(max(1, width)), Rows: uint16(max(1, height))})
		if startErr == nil {
			_ = pty.InheritSize(tty, master)
			_ = eventW.Close()
			_ = ackR.Close()
			return &backend{command: cmd, master: master, tty: tty, ack: ackW, streamNonce: streamNonce, pty: true, processGroup: true}, eventR, master, nil
		}
		// Keep maintenance available even when the host cannot allocate a PTY.
		_ = tty.Close()
		tty = nil
		childEnv = setEnv(childEnv, "DOTS_UP_STREAM_NONCE", "")
		childEnv = setEnv(childEnv, "DOTS_UP_EVENT_FD", "3")
		cmd.ExtraFiles = []*os.File{eventW}
		cmd.Env = setEnv(childEnv, "DOTS_UP_ACK_FD", "")
		cmd.Stdin, cmd.Stdout, cmd.Stderr = os.Stdin, os.Stdout, os.Stderr
		cmd.SysProcAttr = nil
		if err := cmd.Start(); err != nil {
			_ = eventR.Close()
			_ = eventW.Close()
			_ = ackR.Close()
			_ = ackW.Close()
			return nil, nil, nil, err
		}
		fmt.Fprintf(os.Stderr, "dots up: pseudoterminal unavailable (%v); using the current terminal directly\n", startErr)
		_ = eventW.Close()
		_ = ackR.Close()
		_ = ackW.Close()
		return &backend{command: cmd, directTerminal: true}, eventR, nil, nil
	}

	streamNonce, err = newStreamNonce()
	if err != nil {
		_ = eventR.Close()
		_ = eventW.Close()
		_ = ackR.Close()
		_ = ackW.Close()
		return nil, nil, nil, err
	}
	childEnv = setEnv(childEnv, "DOTS_UP_STREAM_NONCE", streamNonce)
	childEnv = setEnv(childEnv, "DOTS_UP_EVENT_FD", "")
	childEnv = setEnv(childEnv, "DOTS_UP_ACK_FD", "")
	cmd.Env = childEnv
	cmd.ExtraFiles = nil
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	var outputR, outputW *os.File
	outputR, outputW, err = os.Pipe()
	if err != nil {
		_ = eventR.Close()
		_ = eventW.Close()
		_ = ackR.Close()
		_ = ackW.Close()
		return nil, nil, nil, err
	}
	cmd.Stdin = nil
	if interactive {
		cmd.Stdin = os.Stdin
	}
	cmd.Stdout, cmd.Stderr = outputW, outputW
	if err := cmd.Start(); err != nil {
		_ = eventR.Close()
		_ = eventW.Close()
		_ = ackR.Close()
		_ = ackW.Close()
		_ = outputR.Close()
		_ = outputW.Close()
		return nil, nil, nil, err
	}
	_ = eventW.Close()
	_ = ackR.Close()
	_ = ackW.Close()
	_ = outputW.Close()
	return &backend{command: cmd, streamNonce: streamNonce, processGroup: true}, eventR, outputR, nil
}

func updaterCommand(opts Options) (*exec.Cmd, error) {
	if runner := os.Getenv("DOTS_UP_RUNNER"); runner != "" {
		if !filepath.IsAbs(runner) {
			return nil, fmt.Errorf("DOTS_UP_RUNNER must be absolute: %s", runner)
		}
		cmd := exec.Command(runner)
		if opts.Command != "" && opts.Command != "up" {
			cmd.Args = append(cmd.Args, opts.Command)
		}
		cmd.Args = append(cmd.Args, opts.Args...)
		return cmd, nil
	}
	mise := os.Getenv("DOTS_MISE_BIN")
	if mise == "" {
		candidate := filepath.Join(os.Getenv("HOME"), ".local", "bin", "mise")
		if _, err := os.Stat(candidate); err == nil {
			mise = candidate
		} else {
			var err error
			mise, err = exec.LookPath("mise")
			if err != nil {
				return nil, errors.New("mise executable was not found")
			}
		}
	}
	if opts.Command == "" || opts.Command == "up" {
		if opts.Home == "" {
			// Transitional: a dots script from before DOTS_HOME runs `dots up`
			// through this renderer with only --root and no capability check.
			// Use the invocation that script expects; remove once every host
			// runs a dots that passes --home.
			cmd := exec.Command(mise, "-C", opts.Root, "run", "setup:update")
			cmd.Args = append(cmd.Args, opts.Args...)
			return cmd, nil
		}
		// dots runs its updater task in the setup's mise environment.
		cmd := exec.Command("bash", filepath.Join(opts.Home, "bin", "dots"), "--backend", "up")
		cmd.Args = append(cmd.Args, opts.Args...)
		return cmd, nil
	}
	if opts.Home == "" {
		return nil, errors.New("the dots manager directory is unknown; pass --home or set DOTS_HOME")
	}
	args := []string{filepath.Join(opts.Home, "lib", "dots-command-backend"), mise, opts.Root, opts.Command}
	args = append(args, opts.Args...)
	return exec.Command("bash", args...), nil
}

func setEnv(env []string, key, value string) []string {
	out := env[:0]
	prefix := key + "="
	for _, entry := range env {
		if !strings.HasPrefix(entry, prefix) {
			out = append(out, entry)
		}
	}
	if value != "" {
		out = append(out, prefix+value)
	}
	return out
}

func readEvents(r *os.File, out chan<- ui.Event, errorsOut chan<- error) {
	defer close(out)
	defer r.Close()
	scanner := bufio.NewScanner(r)
	scanner.Buffer(make([]byte, 4096), 1024*1024)
	for scanner.Scan() {
		line := bytes.TrimSpace(scanner.Bytes())
		if len(line) == 0 {
			continue
		}
		var event ui.Event
		if err := json.Unmarshal(line, &event); err != nil {
			errorsOut <- fmt.Errorf("invalid updater event: %w", err)
			continue
		}
		out <- event
	}
	if err := scanner.Err(); err != nil {
		errorsOut <- fmt.Errorf("read updater events: %w", err)
	}
}

func readControls(r *io.PipeReader, out chan<- controlRecord) {
	defer r.Close()
	scanner := bufio.NewScanner(r)
	scanner.Buffer(make([]byte, 1024), 64*1024)
	for scanner.Scan() {
		var control controlRecord
		if json.Unmarshal(scanner.Bytes(), &control) == nil && control.V == 1 {
			out <- control
		}
	}
}

func readOutput(r io.Reader, nonce string, out chan<- outputRecord) {
	defer close(out)
	if closer, ok := r.(io.Closer); ok {
		defer closer.Close()
	}
	var decoder *eventStreamDecoder
	if nonce != "" {
		var err error
		decoder, err = newEventStreamDecoder(nonce)
		if err != nil {
			out <- outputRecord{err: err}
			return
		}
	}
	send := func(records []outputRecord) {
		for _, record := range records {
			out <- record
		}
	}
	buf := make([]byte, 32*1024)
	for {
		n, err := r.Read(buf)
		if n > 0 {
			if decoder == nil {
				out <- outputRecord{bytes: append([]byte(nil), buf[:n]...)}
			} else {
				send(decoder.feed(buf[:n]))
			}
		}
		if err != nil {
			if decoder != nil {
				send(decoder.flush())
			}
			return
		}
	}
}

func openLog(files *runFiles, name string) *os.File {
	if files == nil {
		return nil
	}
	f, err := os.OpenFile(filepath.Join(files.runDir, name), os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o600)
	if err != nil {
		return nil
	}
	return f
}

func openStageLog(path string) *os.File {
	if path == "" {
		return nil
	}
	f, err := os.OpenFile(path, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o600)
	if err != nil {
		return nil
	}
	return f
}

func writeLog(stage, global *os.File, data []byte) {
	if len(data) == 0 {
		return
	}
	if stage != nil {
		_, _ = stage.Write(data)
	} else if global != nil {
		_, _ = global.Write(data)
	}
}

func cleanTerminalOutput(data []byte) []byte {
	clean := ansi.Strip(string(data))
	clean = strings.ReplaceAll(clean, "\r\n", "\n")
	clean = strings.ReplaceAll(clean, "\r", "\n")
	return []byte(clean)
}

func printProgress(e ui.Event) {
	switch e.Kind {
	case "run_start":
		name := e.Command
		if name == "" {
			name = "up"
		}
		fmt.Printf("dots %s · %s · %s\n", name, e.Profile, e.Host)
	case "stage_start":
		fmt.Printf("→ %s\n", e.Label)
	case "stage_end":
		glyph := "✓"
		if e.Status == "failed" {
			glyph = "×"
		} else if e.Status == "attention" {
			glyph = "!"
		}
		fmt.Printf("%s %s", glyph, e.Label)
		if e.Summary != "" {
			fmt.Printf(" — %s", e.Summary)
		}
		fmt.Println()
	case "notice":
		fmt.Printf("Attention: %s\n", e.Summary)
	case "change":
		fmt.Println(ui.ChangeLine(e.Action, e.Item, e.Before, e.After))
	case "app_store_start":
		fmt.Println("Mac App Store updates · running in the background")
	case "app_store_end":
		fmt.Printf("Mac App Store updates · %s\n", e.Summary)
	}
}

func interactiveTerminal() bool {
	if os.Getenv("CI") != "" || os.Getenv("TERM") == "dumb" {
		return false
	}
	f, err := os.OpenFile("/dev/tty", os.O_RDWR, 0)
	if err != nil {
		return false
	}
	defer f.Close()
	return term.IsTerminal(f.Fd()) && term.IsTerminal(os.Stdin.Fd()) && term.IsTerminal(os.Stdout.Fd())
}

func terminalSize() (int, int) {
	f, err := os.OpenFile("/dev/tty", os.O_RDWR, 0)
	if err != nil {
		return 0, 0
	}
	defer f.Close()
	w, h, err := term.GetSize(f.Fd())
	if err != nil {
		return 0, 0
	}
	return w, h
}

// Reserve the last row so ordinary updater output cannot scroll the back keys away.
func drawManualTerminalHint(tty io.Writer, width, height int, initialize bool, command string) int {
	if width < 2 || height < 2 {
		return 0
	}
	if initialize {
		// The primary screen can resume with its cursor on the bottom row. Make
		// room for the footer, then put output inside the new scrolling region.
		// Saving/restoring that original cursor would strand output on the footer.
		fmt.Fprintf(tty, "\x1b[r\x1b[%d;1H\r\n\x1b[1;%dr\x1b[%d;1H", height, height-1, height-1)
	}
	hint := ansi.Truncate(fmt.Sprintf("← / Esc / q dashboard · dots %s keeps running · Ctrl+] also returns", command), width-1, "")
	// Routine repaint preserves the output cursor and its scrolling margins.
	fmt.Fprintf(tty, "\x1b7\x1b[%d;1H\x1b[0;7m\x1b[2K%s\x1b[0m\x1b8", height, hint)
	return height
}

// Escape needs a short grace period because terminal key sequences can span reads.
const manualEscapeWait = 150 * time.Millisecond

type manualTerminalKeys struct {
	escape []byte
	until  time.Time
}

func (keys *manualTerminalKeys) feed(data []byte) (forward []byte, back bool) {
	for _, b := range data {
		if len(keys.escape) == 0 {
			switch b {
			case 'q', 0x1d: // Ctrl+] remains available for existing users.
				return forward, true
			case 0x1b:
				keys.escape = append(keys.escape, b)
				keys.until = time.Now().Add(manualEscapeWait)
			default:
				forward = append(forward, b)
			}
			continue
		}
		keys.escape = append(keys.escape, b)
		keys.until = time.Now().Add(manualEscapeWait)
		if len(keys.escape) == 2 {
			if b != '[' && b != 'O' {
				return forward, true // Escape followed by another ordinary key.
			}
			continue
		}
		if b >= 0x40 && b <= 0x7e {
			if b == 'D' { // CSI and SS3 left-arrow keys, including modifiers.
				return forward, true
			}
			forward = append(forward, keys.escape...)
			keys.escape = nil
		} else if len(keys.escape) >= 128 {
			// Bound malformed sequences without leaking a possible back key.
			return forward, true
		}
	}
	return forward, false
}

func relayTerminalInput(tty, master *os.File, state *relayState, start <-chan struct{}, manualEnd chan<- struct{}) {
	buf := make([]byte, 256)
	for range start {
		var keys manualTerminalKeys
		for {
			// Quiesce any in-flight terminal read before the dashboard restores terminal state.
			state.inputMu.Lock()
			if !state.active.Load() {
				state.inputMu.Unlock()
				break
			}
			fd := int(tty.Fd())
			var readable unix.FdSet
			readable.Set(fd)
			wait := 100 * time.Millisecond
			if len(keys.escape) > 0 {
				wait = max(0, min(wait, time.Until(keys.until)))
			}
			timeout := unix.NsecToTimeval(int64(wait))
			ready, err := unix.Select(fd+1, &readable, nil, nil, &timeout)
			if !state.active.Load() || err != nil {
				state.inputMu.Unlock()
				continue
			}
			var data []byte
			if ready > 0 && readable.IsSet(fd) {
				n, err := unix.Read(fd, buf)
				if err != nil || n == 0 {
					state.inputMu.Unlock()
					continue
				}
				data = buf[:n]
			}
			manualReturn := false
			if state.manual.Load() {
				if len(data) > 0 {
					data, manualReturn = keys.feed(data)
				} else if len(keys.escape) > 0 && !time.Now().Before(keys.until) {
					manualReturn = true
				}
				if manualReturn {
					state.active.Store(false)
				}
			}
			if len(data) > 0 {
				state.inputEverSent.Store(true)
				state.inputSent.Store(true)
				_, _ = master.Write(data)
			}
			state.inputMu.Unlock()
			if manualReturn {
				select {
				case manualEnd <- struct{}{}:
				default:
				}
				break
			}
		}
	}
}

func cancelBackend(b *backend) {
	if b == nil || b.command == nil || b.command.Process == nil {
		return
	}
	cmd := b.command
	pid := cmd.Process.Pid
	kill := func(signal syscall.Signal) error {
		if b.processGroup {
			return syscall.Kill(-pid, signal)
		}
		return cmd.Process.Signal(signal)
	}
	_ = kill(syscall.SIGINT)
	go func() {
		time.Sleep(150 * time.Millisecond)
		if b.processGroup {
			if syscall.Kill(-pid, 0) != nil {
				return
			}
		} else if cmd.ProcessState != nil && cmd.ProcessState.Exited() {
			return
		}
		_ = kill(syscall.SIGTERM)
		time.Sleep(500 * time.Millisecond)
		_ = kill(syscall.SIGKILL)
	}()
}

func childExitCode(err error) int {
	if err == nil {
		return 0
	}
	var exit *exec.ExitError
	if errors.As(err, &exit) {
		if status, ok := exit.Sys().(syscall.WaitStatus); ok {
			if status.Signaled() {
				return 128 + int(status.Signal())
			}
			return status.ExitStatus()
		}
	}
	return 1
}

func uiRun(events <-chan ui.Event, stateRoot string, noWait bool, control io.Writer) error {
	if control == nil {
		control = io.Discard
	}
	return ui.Run(events, control, stateRoot, noWait)
}
