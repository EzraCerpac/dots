package ui

import (
	"fmt"
	"slices"
	"strings"
	"time"

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

// OutcomeReport prints the same changes and issues as the dashboard.
func OutcomeReport(events []Event, runDir string) string {
	m := NewModel(runDir, false)
	for _, event := range events {
		m.applyEvent(event)
	}
	m.Width = 0 // the terminal wraps printed lines
	status := "dots up · " + m.outcomeStatus()
	if content := m.outcomesContent(true); content != "" {
		return status + "\n" + content
	}
	return status
}

func (m Model) outcomeRows() []int {
	var rows []int
	for i, group := range m.groups() {
		if len(m.groupChanges(group)) > 0 {
			rows = append(rows, i)
		}
	}
	if len(m.issues()) > 0 {
		rows = append(rows, -1)
	}
	return rows
}

func (m Model) outcomeSelection(rows []int) int {
	if len(rows) > 0 && !slices.Contains(rows, m.SelectedGroup) {
		return rows[0]
	}
	return m.SelectedGroup
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

func (s Stage) changeLine(c Change) string {
	if s.ID == "source-sync" && c.Action == "changed" && c.Before == "" && c.After == "" {
		return c.Item
	}
	return ChangeLine(c.Action, c.Item, c.Before, c.After)
}

func (m Model) outcomesContent(plain bool) string {
	var b strings.Builder
	groups := m.groups()
	rows := m.outcomeRows()
	if m.ExpandedGroup >= 0 {
		rows = []int{m.ExpandedGroup}
	}
	for _, i := range rows {
		prefix := ""
		if !plain {
			prefix = "  > "
			if i == m.outcomeSelection(rows) {
				prefix = "› > "
			}
		}
		if i < 0 {
			issues := m.issues()
			fmt.Fprintf(&b, "%s!  Needs attention · %d\n", prefix, len(issues))
			for _, item := range issues {
				for _, line := range strings.Split(m.issueDetail(item), "\n") {
					b.WriteString(wrapIndented("    "+line, m.Width) + "\n")
				}
			}
			continue
		}
		group := groups[i]
		fmt.Fprintln(&b, prefix+group)
		for _, stage := range m.Stages {
			if stageGroup(stage) != group {
				continue
			}
			for _, change := range stage.Changes {
				fmt.Fprintf(&b, "    %s\n", stage.changeLine(change))
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
	groups := m.groups()
	for _, i := range m.outcomeRows() {
		if i == m.SelectedGroup {
			break
		}
		line += 1 + len(m.groupChanges(groups[i]))
	}
	if m.SelectedGroup < 0 {
		m.viewport.SetYOffset(line)
	} else {
		m.viewport.SetYOffset(max(0, line-m.viewport.Height()+1))
	}
}

func (m Model) outcomeStatus() string {
	if m.Finished {
		switch {
		case m.ExitCode == 130:
			return "Cancelled"
		case m.ExitCode != 0:
			return fmt.Sprintf("Failed (exit %d)", m.ExitCode)
		case len(m.issues()) > 0:
			return "Completed with issues"
		case m.changeCount() == 0:
			return "Complete · no verified changes"
		default:
			return "Complete"
		}
	}
	label := "Starting…"
	for _, i := range []int{m.CurrentMain, m.AppStoreIndex} {
		if i >= 0 && m.Stages[i].Status == "running" {
			label = m.Stages[i].Label
			break
		}
	}
	if label == "Starting…" && len(m.Stages) > 0 {
		label = "Updating…"
	}
	return fmt.Sprintf("%s · %s", label, m.Elapsed.Truncate(time.Second))
}

func (m Model) outcomeView() tea.View {
	var b strings.Builder
	fmt.Fprintf(&b, "%s  %s\n", accent.Render("DOTS  /  UP"), muted.Render(strings.TrimSpace(m.Host+"  "+m.Profile)))
	fmt.Fprintln(&b, m.outcomeStatus())
	if m.lastLog != "" {
		b.WriteString(m.viewport.View() + "\n")
	}
	footer := "d diagnostics"
	if len(m.outcomeRows()) > 0 {
		footer += "  ↑↓ select  Enter open  PgUp/PgDn scroll"
	}
	if m.viewport.TotalLineCount() > m.viewport.Height() {
		footer += fmt.Sprintf("  %d%%", int(m.viewport.ScrollPercent()*100))
	}
	if m.ExpandedGroup >= 0 {
		footer = "←/Esc/q back  d diagnostics  ↑↓ scroll"
	}
	if m.ExpandedGroup < 0 {
		if m.Finished {
			footer += "  q close"
		} else {
			footer += "  Ctrl+C cancel  t terminal"
			if m.AppStoreIndex >= 0 {
				footer += "  a App Store"
			}
		}
	}
	if m.Help {
		footer = "Enter opens changes or issues. d shows stages and logs. ←/Esc/q goes back."
	}
	b.WriteString(muted.Render(ansi.Truncate(footer, max(1, m.Width), "…")))
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
