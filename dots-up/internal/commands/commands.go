// Package commands describes the public dots commands without implementing their policies.
package commands

type Spec struct {
	Name, Label, Group, Description, Mode string
	Sensitive, NeedsArgs                  bool
}

var specs = []Spec{
	{"status", "Machine status", "Inspect", "Show files, tools, source and backup health", "scrollback", false, false},
	{"reports", "Saved reports", "Inspect", "Browse completed command reports", "concise", false, false},
	{"up", "Update machine", "Maintain", "Sync configuration and update tools and plugins", "dashboard", false, false},
	{"sync", "Sync configuration", "Maintain", "Share configuration and show changed files", "concise", false, false},
	{"apply", "Apply dotfiles", "Maintain", "Apply declared dotfiles and templates", "scrollback", false, false},
	{"add", "Add software", "Software", "Install and record tools or native packages", "scrollback", false, true},
	{"remove", "Remove software", "Software", "Uninstall software and remove declarations", "scrollback", false, true},
	{"backup", "Back up settings", "Settings", "Save and publish encrypted settings", "scrollback", false, false},
	{"restore", "Restore settings", "Settings", "Restore private settings history", "scrollback", true, false},
	{"atuin-login", "Atuin login", "Settings", "Finish Atuin login and authentication", "scrollback", true, false},
	{"atuin-bundle", "Atuin bundle", "Settings", "Prepare the private Atuin bundle", "scrollback", true, false},
	{"bootstrap", "Bootstrap machine", "Setup", "Install and apply declared machine setup", "scrollback", true, false},
	{"remote", "Bootstrap remote host", "Setup", "Bootstrap a machine over existing SSH", "scrollback", true, true},
	{"bundle", "Private bundle", "Setup", "Create or check encrypted bootstrap inputs", "scrollback", true, true},
	{"test", "Run tests", "Development", "Run repository test suites", "scrollback", false, false},
	{"plan", "Native setup plan", "Advanced", "Print the existing native mise plan", "concise", false, false},
}

func Canonical(name string) string {
	switch name {
	case "update":
		return "up"
	case "uninstall":
		return "remove"
	}
	return name
}
func Lookup(name string) (Spec, bool) {
	name = Canonical(name)
	for _, s := range specs {
		if s.Name == name {
			return s, true
		}
	}
	return Spec{}, false
}
func All() []Spec { return append([]Spec(nil), specs...) }
