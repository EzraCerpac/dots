package supervisor

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/EzraCerpac/dots/dots-up/internal/ui"
)

type Report struct {
	Version    int        `json:"version"`
	Command    string     `json:"command,omitempty"`
	ID         string     `json:"id"`
	Profile    string     `json:"profile,omitempty"`
	Host       string     `json:"host,omitempty"`
	StartedAt  time.Time  `json:"started_at"`
	FinishedAt time.Time  `json:"finished_at"`
	ExitCode   int        `json:"exit_code"`
	Events     []ui.Event `json:"events"`
}

type runFiles struct {
	stateDir string
	runDir   string
	eventLog *os.File
	report   Report
}

func stateDir() string {
	if path := os.Getenv("DOTS_UP_STATE_DIR"); path != "" {
		return path
	}
	base := os.Getenv("XDG_STATE_HOME")
	if base == "" {
		base = filepath.Join(os.Getenv("HOME"), ".local", "state")
	}
	return filepath.Join(base, "dots", "up")
}

func newRunFiles(now time.Time, stateRoot string) (*runFiles, error) {
	if stateRoot == "" {
		stateRoot = stateDir()
	}
	if !filepath.IsAbs(stateRoot) {
		return nil, fmt.Errorf("state directory must be absolute: %s", stateRoot)
	}
	if err := os.MkdirAll(stateRoot, 0o700); err != nil {
		return nil, fmt.Errorf("create update state directory: %w", err)
	}
	if err := os.Chmod(stateRoot, 0o700); err != nil {
		return nil, fmt.Errorf("secure update state directory: %w", err)
	}
	id := fmt.Sprintf("%s-%d", now.UTC().Format("20060102T150405Z"), os.Getpid())
	runDir := filepath.Join(stateRoot, id)
	if err := os.Mkdir(runDir, 0o700); err != nil {
		return nil, fmt.Errorf("create update report directory: %w", err)
	}
	if err := os.Mkdir(filepath.Join(runDir, "stages"), 0o700); err != nil {
		return nil, fmt.Errorf("create stage log directory: %w", err)
	}
	active, err := os.OpenFile(filepath.Join(runDir, ".active"), os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
	if err != nil {
		return nil, fmt.Errorf("mark active update report: %w", err)
	}
	_ = active.Close()
	events, err := os.OpenFile(filepath.Join(runDir, "events.jsonl"), os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
	if err != nil {
		return nil, fmt.Errorf("create update event log: %w", err)
	}
	return &runFiles{
		stateDir: stateRoot,
		runDir:   runDir,
		eventLog: events,
		report:   Report{Version: 1, ID: id, StartedAt: now.UTC(), Events: make([]ui.Event, 0, 64)},
	}, nil
}

func (r *runFiles) logPath(id string) string {
	id = safeID(id)
	if id == "" {
		id = "stage"
	}
	return filepath.Join(r.runDir, "stages", id+".log")
}

func safeID(s string) string {
	var b strings.Builder
	for _, r := range s {
		if (r >= 'a' && r <= 'z') || (r >= 'A' && r <= 'Z') || (r >= '0' && r <= '9') || r == '-' || r == '_' {
			b.WriteRune(r)
		}
	}
	return b.String()
}

func (r *runFiles) record(e ui.Event) error {
	if e.LogPath != "" && !inside(r.runDir, e.LogPath) {
		return errors.New("event log path escapes the update report directory")
	}
	// Keep the live outcome even if the event log cannot accept this event.
	if e.Kind == "run_start" {
		r.report.Command = e.Command
	}
	r.report.Events = append(r.report.Events, e)
	if err := json.NewEncoder(r.eventLog).Encode(e); err != nil {
		return fmt.Errorf("write update event log: %w", err)
	}
	return nil
}

func (r *runFiles) finish(code int, profile, host string, now time.Time) error {
	r.report.Profile = profile
	r.report.Host = host
	r.report.ExitCode = code
	r.report.FinishedAt = now.UTC()
	if err := r.eventLog.Sync(); err != nil {
		return fmt.Errorf("sync update event log: %w", err)
	}
	if err := r.eventLog.Close(); err != nil {
		return fmt.Errorf("close update event log: %w", err)
	}
	path := filepath.Join(r.runDir, "report.json")
	data, err := json.MarshalIndent(r.report, "", "  ")
	if err != nil {
		return fmt.Errorf("encode update report: %w", err)
	}
	if err := writePrivate(path, data); err != nil {
		return fmt.Errorf("write update report: %w", err)
	}
	if err := os.Remove(filepath.Join(r.runDir, ".active")); err != nil && !errors.Is(err, os.ErrNotExist) {
		return fmt.Errorf("mark update report complete: %w", err)
	}
	return pruneReports(r.stateDir, 10)
}

func writePrivate(path string, data []byte) error {
	tmp, err := os.CreateTemp(filepath.Dir(path), ".report-*.tmp")
	if err != nil {
		return err
	}
	name := tmp.Name()
	defer os.Remove(name)
	if err := tmp.Chmod(0o600); err != nil {
		_ = tmp.Close()
		return err
	}
	if _, err := tmp.Write(data); err != nil {
		_ = tmp.Close()
		return err
	}
	if err := tmp.Sync(); err != nil {
		_ = tmp.Close()
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	return os.Rename(name, path)
}

func inside(root, path string) bool {
	if !filepath.IsAbs(path) {
		return false
	}
	rel, err := filepath.Rel(root, path)
	return err == nil && rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator))
}

func pruneReports(root string, keep int) error {
	entries, err := os.ReadDir(root)
	if err != nil {
		return err
	}
	type saved struct {
		path string
		when time.Time
	}
	completed := make([]saved, 0, len(entries))
	for _, entry := range entries {
		if !entry.IsDir() || entry.Type()&os.ModeSymlink != 0 {
			continue
		}
		path := filepath.Join(root, entry.Name())
		if _, err := os.Stat(filepath.Join(path, ".active")); err == nil {
			continue
		}
		report, err := readReport(path)
		if err != nil || report.FinishedAt.IsZero() {
			continue
		}
		completed = append(completed, saved{path: path, when: report.FinishedAt})
	}
	sort.Slice(completed, func(i, j int) bool { return completed[i].when.After(completed[j].when) })
	for _, item := range completed[min(keep, len(completed)):] {
		if err := os.RemoveAll(item.path); err != nil {
			return fmt.Errorf("remove old update report %s: %w", filepath.Base(item.path), err)
		}
	}
	return nil
}

func readReport(dir string) (Report, error) {
	var report Report
	data, err := os.ReadFile(filepath.Join(dir, "report.json"))
	if err != nil {
		return report, err
	}
	if err := json.Unmarshal(data, &report); err != nil {
		return report, err
	}
	if report.Version != 1 || report.ID == "" || report.FinishedAt.IsZero() {
		return Report{}, errors.New("invalid update report")
	}
	return report, nil
}

func latestReport(root string) (Report, string, error) {
	entries, err := os.ReadDir(root)
	if err != nil {
		return Report{}, "", err
	}
	var best Report
	bestDir := ""
	for _, entry := range entries {
		if !entry.IsDir() || entry.Type()&os.ModeSymlink != 0 {
			continue
		}
		dir := filepath.Join(root, entry.Name())
		if _, err := os.Stat(filepath.Join(dir, ".active")); err == nil {
			continue
		}
		report, err := readReport(dir)
		if err == nil && report.FinishedAt.After(best.FinishedAt) {
			best, bestDir = report, dir
		}
	}
	if bestDir == "" {
		return Report{}, "", os.ErrNotExist
	}
	return best, bestDir, nil
}

func replayEvents(report Report) <-chan ui.Event {
	out := make(chan ui.Event, len(report.Events)+1)
	for _, event := range report.Events {
		if event.Kind == "run_start" && event.Command == "" {
			event.Command = report.Command
		}
		// Reports replay history only; no updater exists to receive a handoff.
		if event.Kind == "stage_start" && event.Terminal == "handoff" {
			event.Terminal = "hidden"
		}
		if event.Kind == "handoff_end" || event.Kind == "terminal_end" {
			continue
		}
		out <- event
	}
	if len(report.Events) == 0 || report.Events[len(report.Events)-1].Kind != "run_end" {
		out <- ui.Event{V: 1, Kind: "run_end", ExitCode: report.ExitCode, DurationMS: report.FinishedAt.Sub(report.StartedAt).Milliseconds()}
	}
	close(out)
	return out
}

func summary(report Report, dir string) string {
	var result string
	switch {
	case report.ExitCode == 130:
		result = "dots " + report.Command + " cancelled locally"
	case report.ExitCode != 0:
		result = fmt.Sprintf("dots %s failed (exit %d)", report.Command, report.ExitCode)
	default:
		result = "dots " + report.Command + " completed"
		for _, e := range report.Events {
			if (e.Kind == "notice" && (e.Severity == "attention" || e.Severity == "error")) ||
				(e.Kind == "stage_end" && (e.Status == "failed" || e.Status == "attention")) {
				result = "dots " + report.Command + " needs attention"
				break
			}
		}
	}
	return fmt.Sprintf("%s\nReport: %s", result, dir)
}

func printUpReport(w io.Writer, report Report, dir string, lastHint bool) {
	events := make([]ui.Event, 0, len(report.Events)+1)
	for event := range replayEvents(report) {
		events = append(events, event)
	}
	fmt.Fprintln(w, ui.OutcomeReport(events, dir))
	if lastHint {
		fmt.Fprintln(w, "Details and logs · dots up --last")
	}
}

func ShowLast(opts Options) int {
	root := opts.StateDir
	if root == "" {
		root = commandStateDir(opts.Command)
	}
	report, dir, err := latestReport(root)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			fmt.Fprintln(os.Stderr, "dots up: no completed update report is available yet")
		} else {
			fmt.Fprintf(os.Stderr, "dots up: read latest report: %v\n", err)
		}
		return 1
	}
	if interactiveTerminal() && !opts.Plain && !opts.Verbose {
		err := uiRun(replayEvents(report), root, opts.NoWait, nil)
		if err != nil {
			fmt.Fprintf(os.Stderr, "dots up: dashboard unavailable: %v\n", err)
		}
	}
	if report.Command == "" || report.Command == "up" {
		printUpReport(os.Stdout, report, dir, false)
		return report.ExitCode
	}
	fmt.Println(summary(report, dir))
	if opts.Plain || opts.Verbose || !interactiveTerminal() {
		if report.Command == "status" {
			for event := range replayEvents(report) {
				printStatusEvent(event)
			}
		}
		printReportHighlights(os.Stdout, report, report.Command == "status")
	}
	return report.ExitCode
}

func printReportHighlights(w io.Writer, report Report, statusInventoryShown bool) {
	labels := map[string]string{}
	for _, e := range report.Events {
		if e.Kind == "stage_start" {
			labels[e.ID] = e.Label
		}
	}
	// Steps whose problems are listed as their own items.
	noticed := map[string]bool{}
	logs := map[string]string{}
	for _, e := range report.Events {
		if e.ID != "" && e.LogPath != "" {
			logs[e.ID] = e.LogPath
		}
		if e.Kind == "notice" && e.Stage != "" && (e.Severity == "attention" || e.Severity == "error") {
			noticed[e.Stage] = true
		}
	}
	for _, e := range report.Events {
		switch e.Kind {
		case "item_upsert":
			if report.Command == "status" && (statusInventoryShown || e.Status == "ok" || e.Status == "skipped") {
				continue
			}
			fmt.Fprintf(w, "  %s  %s — %s\n", e.Label, e.Status, e.Summary)
		case "change":
			fmt.Fprintln(w, "  "+ui.ChangeLine(e.Action, e.Item, e.Before, e.After))
		case "notice":
			if e.Severity == "attention" || e.Severity == "error" {
				printIssue(w, "Attention: "+e.Summary, e.Detail, e.Action, logs[e.Stage])
			}
		case "stage_end":
			if e.Status == "failed" || e.Status == "attention" {
				label := labels[e.ID]
				if label == "" {
					label = e.Label
				}
				kind := "Failed"
				if e.Status == "attention" {
					kind = "Needs attention"
				}
				detail := e.Detail
				if detail == "" && !noticed[e.ID] {
					detail = ui.LogTail(e.LogPath)
				}
				printIssue(w, fmt.Sprintf("%s: %s — %s", kind, label, e.Summary), detail, e.Action, e.LogPath)
			}
		}
	}
	for _, e := range report.Events {
		if e.LogPath != "" {
			fmt.Fprintf(w, "Logs: %s\n", filepath.Dir(e.LogPath))
			return
		}
	}
}

func printStatusEvent(e ui.Event) {
	switch e.Kind {
	case "run_start":
		name := e.Command
		if name == "" {
			name = "up"
		}
		fmt.Printf("dots %s · %s · %s\n", ui.CleanLine(name), ui.CleanLine(e.Profile), ui.CleanLine(e.Host))
	case "item_upsert":
		fmt.Printf("  %s · %s  %s — %s\n", ui.CleanLine(e.Group), ui.CleanLine(e.Label), ui.CleanLine(e.Status), ui.CleanLine(e.Summary))
		if e.Action != "" && e.Status != "ok" && e.Status != "skipped" {
			fmt.Printf("    %s\n", ui.CleanLine(e.Action))
		}
	}
}

// printIssue prints one attention item with its error, next step and log.
func printIssue(w io.Writer, text, detail, action, logPath string) {
	for _, line := range strings.Split(ui.IssueDetail(text, detail, action, logPath), "\n") {
		fmt.Fprintln(w, strings.TrimRight("  "+line, " "))
	}
}

func tailFile(path string, maxBytes int64) string {
	f, err := os.Open(path)
	if err != nil {
		return ""
	}
	defer f.Close()
	st, err := f.Stat()
	if err != nil {
		return ""
	}
	start := max(int64(0), st.Size()-maxBytes)
	if _, err := f.Seek(start, io.SeekStart); err != nil {
		return ""
	}
	s := bufio.NewScanner(f)
	s.Buffer(make([]byte, 4096), int(maxBytes))
	var lines []string
	for s.Scan() {
		lines = append(lines, s.Text())
	}
	if start > 0 && len(lines) > 0 {
		lines[0] = "… earlier output omitted … " + lines[0]
	}
	return strings.Join(lines, "\n")
}

// commandStateDir preserves legacy update reports and separates other commands.
func commandStateDir(command string) string {
	if command == "" || command == "up" {
		return stateDir()
	}
	base := os.Getenv("DOTS_STATE_DIR")
	if base == "" {
		base = os.Getenv("XDG_STATE_HOME")
		if base == "" {
			base = filepath.Join(os.Getenv("HOME"), ".local", "state")
		}
		base = filepath.Join(base, "dots")
	}
	return filepath.Join(base, safeID(command))
}
