package ui

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"

	tea "charm.land/bubbletea/v2"
)

// issueOutputLines is how many lines of the error an issue shows; the full
// log stays one path away.
const issueOutputLines = 6

// IssueDetail is everything one attention item says, in this order: what
// failed, the last lines of the error (DETAIL, the output the step reported
// with it), the one command to run next, and the
// log. The dashboard, plain output and saved reports all show it.
func IssueDetail(text, detail, action, logPath string) string {
	lines := []string{text}
	for _, line := range lastLines(detail, issueOutputLines) {
		lines = append(lines, "    "+line)
	}
	if action != "" {
		lines = append(lines, "    Next: "+action)
	}
	if logPath != "" {
		lines = append(lines, "    Log: "+logPath)
	}
	return strings.Join(lines, "\n")
}

// LogTail is the end of a step's log, for a step that reported no output of
// its own. A step whose problems are listed separately does not use it: the
// log holds their output, not this item's.
func LogTail(logPath string) string {
	if logPath == "" {
		return ""
	}
	if _, err := os.Stat(logPath); err != nil {
		return ""
	}
	return readTail(logPath)
}

// lastLines returns the last LIMIT non-empty lines of TEXT.
func lastLines(text string, limit int) []string {
	var lines []string
	for _, line := range strings.Split(text, "\n") {
		if strings.TrimSpace(line) != "" && line != "Waiting for output…" {
			lines = append(lines, strings.TrimRight(line, " \t"))
		}
	}
	return lines[max(0, len(lines)-limit):]
}

func (m Model) issueDetail(item issue) string {
	detail := item.detail
	if detail == "" && !item.notice && !m.stageHasNotice(item.stage) {
		detail = LogTail(item.logPath)
	}
	return IssueDetail(item.text, detail, item.action, item.logPath)
}

func (m Model) stageHasNotice(stage int) bool {
	if stage < 0 || stage >= len(m.Stages) {
		return false
	}
	for _, n := range m.Notices {
		// A notice that only repeats the step's summary is not listed separately.
		if n.Stage == m.Stages[stage].ID && (n.Severity == "attention" || n.Severity == "error") &&
			!strings.EqualFold(n.Summary, m.Stages[stage].Summary) {
			return true
		}
	}
	return false
}

// AgentCommandEnv names the shell command that receives an issue as its final
// argument, for example `claude` or `codex`.
const AgentCommandEnv = "DOTS_AGENT_COMMAND"

type agentDoneMsg struct{ err error }

// agentPrompt is the issue as a self-contained task for a coding agent.
func (m Model) agentPrompt(item issue) string {
	root := os.Getenv("DOTS_ROOT")
	home := os.Getenv("DOTS_HOME")
	version := "unknown"
	if home != "" {
		version = "dev (checkout)"
		if data, err := os.ReadFile(filepath.Join(home, "VERSION")); err == nil && strings.TrimSpace(string(data)) != "" {
			version = strings.TrimSpace(string(data))
		}
	}
	var b strings.Builder
	b.WriteString("`dots " + m.commandName() + "` reported an issue on this machine. Find the cause and fix it in the configuration, or tell me exactly what I must do.\n\n")
	b.WriteString(m.issueDetail(item) + "\n\n")
	fmt.Fprintf(&b, "Configuration root (DOTS_ROOT): %s\n", root)
	fmt.Fprintf(&b, "Host: %s, profile: %s\n", m.Host, m.Profile)
	fmt.Fprintf(&b, "dots %s, manager at %s; its skill is skills/dots/SKILL.md there.\n", version, home)
	b.WriteString("Do not run dots up or dots sync yourself; tell me when to rerun.\n")
	return b.String()
}

func (m Model) commandName() string {
	if m.Command == "" {
		return "up"
	}
	return m.Command
}

// handToAgent suspends the dashboard and runs DOTS_AGENT_COMMAND with the
// selected issue's prompt as its final argument, in the configuration root.
// It waits for the run to finish, so the agent never works in a checkout the
// update still holds.
func (m *Model) handToAgent() tea.Cmd {
	item := m.selectedIssue()
	if item == nil {
		return nil
	}
	if !m.Finished {
		m.AgentMessage = "The agent hand-off is available once this run finishes; the update still holds the configuration checkout."
		return nil
	}
	command := strings.TrimSpace(os.Getenv(AgentCommandEnv))
	if command == "" {
		m.AgentMessage = "Set " + AgentCommandEnv + " to hand an issue to a coding agent, for example DOTS_AGENT_COMMAND=claude or DOTS_AGENT_COMMAND=codex; it receives the issue as its last argument."
		return nil
	}
	m.AgentMessage = ""
	return tea.ExecProcess(agentCommand(command, m.agentPrompt(*item)), func(err error) tea.Msg { return agentDoneMsg{err} })
}

func agentCommand(command, prompt string) *exec.Cmd {
	cmd := exec.Command("sh", "-c", command+` "$1"`, "dots-agent", prompt)
	if root := os.Getenv("DOTS_ROOT"); root != "" {
		cmd.Dir = root
	}
	return cmd
}
