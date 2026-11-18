package supervisor

import (
	"io"
	"os"
	"os/exec"
	"syscall"
	"time"

	"github.com/charmbracelet/x/term"
)

const scrollbackDrain = 250 * time.Millisecond

func startScrollback(cmd *exec.Cmd, files *runFiles, sensitive bool) (*backend, error) {
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
	if !sensitive && (!stdoutTTY || !stderrTTY) {
		child.scrollbackLog = openLog(files, "backend.log")
		if log := child.scrollbackLog; log != nil {
			// Native terminal streams belong to the child. Capture only streams
			// that are already redirected, keeping their destinations separate.
			if !stdoutTTY {
				cmd.Stdout = scrollbackOutput{os.Stdout, log}
			}
			if !stderrTTY {
				cmd.Stderr = scrollbackOutput{os.Stderr, log}
			}
			cmd.WaitDelay = scrollbackDrain
		}
	}
	if err := cmd.Start(); err != nil {
		if child.scrollbackLog != nil {
			_ = child.scrollbackLog.Close()
		}
		return nil, err
	}
	return child, nil
}

type scrollbackOutput struct {
	output io.Writer
	log    *os.File
}

func (w scrollbackOutput) Write(data []byte) (int, error) {
	n, err := w.output.Write(data)
	writeLog(nil, w.log, cleanTerminalOutput(data[:n]))
	return n, err
}
