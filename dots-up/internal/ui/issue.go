package ui

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"

	tea "charm.land/bubbletea/v2"
)

// issueOutputLines is how much of a stage log an issue shows; the full log
// stays one path away.
const issueOutputLines = 30

// IssueDetail is everything one attention item says: what happened, the end
// of the output that explains it, where the full log is, and what to do next.
// The dashboard, plain output and saved reports all show it.
func IssueDetail(text, action, logPath string) string {
	var b strings.Builder
	b.WriteString(text + "\n")
	if output := logTail(logPath, issueOutputLines); output != "" {
		b.WriteString("\nOutput (end of the log):\n")
		for _, line := range strings.Split(output, "\n") {
			b.WriteString("  " + line + "\n")
		}
	}
	if logPath != "" {
		b.WriteString("\nFull log: " + logPath + "\n")
	}
	if action != "" {
		b.WriteString("Next: " + action + "\n")
	}
	return strings.TrimRight(b.String(), "\n")
}

// logTail returns the last LIMIT non-empty lines of a log, or "" when there is
// none to show.
func logTail(path string, limit int) string {
	if path == "" {
		return ""
	}
	if _, err := os.Stat(path); err != nil {
		return ""
	}
	var lines []string
	for _, line := range strings.Split(readTail(path), "\n") {
		if strings.TrimSpace(line) != "" && line != "Waiting for output…" {
			lines = append(lines, strings.TrimRight(line, " \t"))
		}
	}
	return strings.Join(lines[max(0, len(lines)-limit):], "\n")
}

func (m Model) issueDetail(item issue) string {
	return IssueDetail(item.text, item.action, item.logPath)
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
