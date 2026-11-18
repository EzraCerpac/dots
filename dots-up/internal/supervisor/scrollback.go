package supervisor

import (
	"os"
	"os/exec"
	"syscall"
	"time"

	"github.com/charmbracelet/x/term"
)

const scrollbackDrain = 250 * time.Millisecond

func startScrollback(cmd *exec.Cmd) (*backend, error) {
	cmd.Stdin, cmd.Stdout, cmd.Stderr = os.Stdin, os.Stdout, os.Stderr
	child := &backend{command: cmd, directTerminal: true}
	stdoutTTY, stderrTTY := term.IsTerminal(os.Stdout.Fd()), term.IsTerminal(os.Stderr.Fd())
	// Keep the foreground process group when there is a controlling terminal.
	// Programs such as sudo and ssh may read it even with redirected stdio.
	if !stdoutTTY && !stderrTTY && !term.IsTerminal(os.Stdin.Fd()) {
		if tty, err := os.OpenFile("/dev/tty", os.O_RDWR, 0); err == nil {
			_ = tty.Close()
		} else {
			cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
			child.processGroup = true
		}
	}
	if err := cmd.Start(); err != nil {
		return nil, err
	}
	return child, nil
}
