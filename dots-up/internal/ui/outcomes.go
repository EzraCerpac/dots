package ui

import (
	"fmt"
	"strings"

	tea "charm.land/bubbletea/v2"
	"github.com/charmbracelet/x/ansi"
)

// ChangeAction also interprets reports saved before actions were recorded.
func ChangeAction(action, before, after string) string {
	switch action {
	case "installed", "updated", "removed", "changed":
		return action
	}
	missing := func(s string) bool { return s == "" || s == "not installed" || s == "not present" }
	if missing(before) && !missing(after) {
		return "installed"
	}
	if !missing(before) && missing(after) {
		return "removed"
	}
	if before != "" && after != "" {
		return "updated"
	}
	return "changed"
}

func ChangeLine(action, item, before, after string) string {
	action = ChangeAction(action, before, after)
	line := action + "  " + item
	switch action {
	case "installed":
		if after != "" {
			line += "  " + after
		}
	case "removed":
		if before != "" {
			line += "  " + before
		}
	default:
		if before != "" || after != "" {
			line += "  " + before + " → " + after
		}
	}
	return line
}

func (m Model) isUp() bool { return m.Command == "" || m.Command == "up" }

// OutcomeReport is the printed form of the outcomes area of the run in
// RUNDIR, without areas that have nothing to report.
func OutcomeReport(events []Event, runDir string) string {
	m := NewModel(runDir, false)
	for _, event := range events {
		m.apply(event)
	}
	m.Width = 0 // the terminal wraps printed lines
	var lines []string
	for _, line := range strings.Split(m.outcomesContent(), "\n") {
		if area, ok := strings.CutPrefix(line, "› > "); ok {
			line = area
		} else if area, ok := strings.CutPrefix(line, "  > "); ok {
			line = area
		} else {
			lines = append(lines, line)
			continue
		}
		if strings.HasSuffix(line, " · no verified changes") {
			continue
		}
		lines = append(lines, line)
	}
	return strings.Join(lines, "\n")
}
func (m Model) outcomesView() bool {
	return m.isUp() && !m.Diagnostics && !m.AttentionExpanded && m.overview()
}

func (m Model) groupChanges(group string) []Change {
	var changes []Change
	for _, stage := range m.Stages {
		if stageGroup(stage) == group {
			changes = append(changes, stage.Changes...)
		}
	}
	return changes
}

func (m Model) groupOutcome(group string) string {
	counts := map[string]int{}
	for _, c := range m.groupChanges(group) {
		counts[ChangeAction(c.Action, c.Before, c.After)]++
	}
	var parts []string
	for _, action := range []string{"installed", "updated", "removed", "changed"} {
		if counts[action] > 0 {
			parts = append(parts, fmt.Sprintf("%d %s", counts[action], action))
		}
	}
	failed, attention, running, total := 0, 0, false, 0
	for _, stage := range m.Stages {
		if stageGroup(stage) != group {
			continue
		}
		total++
		switch stage.Status {
		case "failed":
			failed++
		case "attention", "missing", "drift", "unknown":
			attention++
		case "running":
			running = true
		}
	}
	if len(parts) == 0 {
		text := "no verified changes"
		if total == 0 && !m.Finished {
			text = "waiting"
		}
		parts = append(parts, text)
	}
	if running {
		parts = append(parts, "running")
	}
	if failed > 0 {
		parts = append(parts, fmt.Sprintf("%d failed", failed))
	}
	if attention > 0 {
		parts = append(parts, fmt.Sprintf("%d attention", attention))
	}
	return strings.Join(parts, " · ")
}

func (m Model) outcomesContent() string {
	var b strings.Builder
	groups := m.groups()
	for i, group := range groups {
		if m.ExpandedGroup >= 0 && i != m.ExpandedGroup {
			continue
		}
		prefix := "  > "
		if i == m.SelectedGroup {
			prefix = "› > "
		}
		fmt.Fprintf(&b, "%s%s · %s\n", prefix, group, m.groupOutcome(group))
		for _, c := range m.groupChanges(group) {
			fmt.Fprintf(&b, "    %s\n", ChangeLine(c.Action, c.Item, c.Before, c.After))
		}
	}
	if m.ExpandedGroup < 0 {
		prefix := "  > "
		if m.SelectedGroup < 0 {
			prefix = "› > "
		}
		issues := m.issues()
		fmt.Fprintf(&b, "%s!  Needs attention · %d", prefix, len(issues))
		for _, item := range issues {
			for _, line := range strings.Split(m.issueDetail(item), "\n") {
				b.WriteString("\n" + wrapIndented("    "+line, m.Width))
			}
		}
	}
	return strings.TrimRight(b.String(), "\n")
}

// wrapIndented wraps LINE to WIDTH (0: not at all), continuing under its
// indentation, so an error line is read whole.
func wrapIndented(line string, width int) string {
	text := strings.TrimLeft(line, " ")
	indent := strings.Repeat(" ", len(line)-len(text)+2)
	if width <= len(indent)+20 || ansi.StringWidth(line) <= width {
		return line
	}
	return line[:len(indent)-2] + strings.ReplaceAll(ansi.Wrap(text, width-len(indent), ""), "\n", "\n"+indent)
}

func (m *Model) scrollToOutcomeGroup() {
	line := 0
	for i, group := range m.groups() {
		if i == m.SelectedGroup {
			break
		}
		line += 1 + len(m.groupChanges(group))
	}
	if m.SelectedGroup < 0 {
		m.viewport.SetYOffset(line) // the issues start at the top
		return
	}
	if line >= m.viewport.Height() {
		m.viewport.ScrollDown(line - m.viewport.Height() + 1)
	}
}

func (m Model) outcomeView() tea.View {
	status := "Running"
	if m.Finished {
		status = "Complete"
		if m.ExitCode != 0 || len(m.issues()) > 0 {
			status = "Needs attention"
		}
	}
	var b strings.Builder
	fmt.Fprintf(&b, "%s  %s\n", accent.Render("DOTS  /  UP"), muted.Render(strings.TrimSpace(m.Host+"  "+m.Profile)))
	fmt.Fprintf(&b, "%s · %d verified changes · %d need attention\n", status, m.changeCount(), len(m.issues()))
	if m.ExpandedGroup >= 0 {
		b.WriteString(accent.Render("AREA OUTCOMES") + "\n")
	} else {
		b.WriteString(accent.Render("OUTCOMES") + "\n")
	}
	b.WriteString(m.viewport.View() + "\n")
	footer := "↑↓ areas  Enter open  PgUp/PgDn scroll"
	if m.viewport.TotalLineCount() > m.viewport.Height() {
		footer += fmt.Sprintf("  %d%%", int(m.viewport.ScrollPercent()*100))
	}
	if m.ExpandedGroup >= 0 {
		footer = "↑↓/PgUp/PgDn scroll  d diagnostics  ←/Esc/q back"
	}
	if m.ExpandedGroup < 0 {
		if m.Finished {
			footer += "  q close"
		} else {
			footer += "  Ctrl+C cancel  t terminal"
		}
	}
	if m.Help {
		footer = "Open an area for all changes; d shows its stages and logs. ←/Esc/q returns one level."
	}
	b.WriteString(muted.Render(ansi.Truncate(footer, max(1, m.Width), "…")))
	if m.Finished {
		b.WriteString("\n" + accent.Render("Finished"))
	}
	lines := strings.Split(b.String(), "\n")
	for i := range lines {
		lines[i] = ansi.Truncate(lines[i], max(1, m.Width), "…")
		if m.Width < 55 || m.Height < 18 {
			lines[i] = ansi.Strip(lines[i])
		}
	}
	v := tea.NewView(strings.Join(lines, "\n"))
	v.AltScreen = true
	return v
}
