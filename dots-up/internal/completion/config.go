package completion

import (
	"context"
	"encoding/json"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"time"

	"github.com/pelletier/go-toml/v2"
)

func readRegular(path string, limit int64) []byte {
	info, err := os.Stat(path)
	if err != nil || !info.Mode().IsRegular() || info.Size() > limit {
		return nil
	}
	file, err := os.Open(path)
	if err != nil {
		return nil
	}
	defer file.Close()
	data, err := io.ReadAll(io.LimitReader(file, limit+1))
	if err != nil || int64(len(data)) > limit {
		return nil
	}
	return data
}

type table = map[string]any

func readConfig(path string) table {
	var doc table
	if data := readRegular(path, 2<<20); data != nil && toml.Unmarshal(data, &doc) == nil {
		return doc
	}
	return nil
}

func subtable(doc table, name string) table {
	value, _ := doc[name].(table)
	return value
}

var profileName = regexp.MustCompile(`^[a-z0-9][a-z0-9-]*$`)

func profiles(root string) []string {
	doc := readConfig(filepath.Join(root, "config.toml"))
	declared, _ := subtable(doc, "vars")["dots_profiles"].(string)
	names := strings.Fields(declared)
	for _, name := range names {
		if !profileName.MatchString(name) || name == "base" || name == "local" || strings.HasPrefix(name, "host-") {
			return nil
		}
	}
	return names
}

func miseBinary() string {
	// Match bin/dots: the override or standalone installation wins over PATH.
	binary := os.Getenv("DOTS_MISE_BIN")
	if binary == "" {
		binary = filepath.Join(os.Getenv("HOME"), ".local", "bin", "mise")
	}
	if info, err := os.Stat(binary); err == nil && info.Mode().IsRegular() && info.Mode().Perm()&0111 != 0 {
		absolute, _ := filepath.Abs(binary)
		return absolute
	}
	binary, _ = exec.LookPath("mise")
	return binary
}

var configProbeTimeout = 1200 * time.Millisecond

func activeConfigs(root string) []string {
	binary := miseBinary()
	if binary == "" {
		return nil
	}
	// mise tracks loaded files in its state directory. Give the probe disposable
	// state and cache directories so even first-time discovery leaves no records.
	scratch, err := os.MkdirTemp("", "dots-completion-")
	if err != nil {
		return nil
	}
	defer os.RemoveAll(scratch)

	ctx, cancel := context.WithTimeout(context.Background(), configProbeTimeout)
	defer cancel()
	cmd := exec.CommandContext(ctx, binary, "-C", root, "config", "ls", "--json")
	cmd.Dir = root
	cmd.WaitDelay = 50 * time.Millisecond
	env := slices.DeleteFunc(os.Environ(), func(entry string) bool {
		return strings.HasPrefix(entry, "MISE_ENV=") || strings.HasPrefix(entry, "MISE_LOG_FILE=")
	})
	cmd.Env = append(env,
		"MISE_CONFIG_DIR="+root, "MISE_OFFLINE=1", "MISE_SAFE=1", "MISE_NO_ENV=1",
		"MISE_NO_HOOKS=1", "MISE_AUTO_INSTALL=0", "MISE_EXEC_AUTO_INSTALL=0",
		"MISE_AUTO_UPDATE=0", "MISE_CACHE_DIR="+filepath.Join(scratch, "cache"),
		"MISE_STATE_DIR="+filepath.Join(scratch, "state"),
	)
	output, err := cmd.Output()
	if err != nil {
		return nil
	}
	var listed []struct{ Path string }
	if json.Unmarshal(output, &listed) != nil {
		return nil
	}
	var files []string
	for _, entry := range listed {
		if !filepath.IsAbs(entry.Path) {
			continue
		}
		path, err := filepath.EvalSymlinks(entry.Path)
		if err != nil {
			continue
		}
		relative, err := filepath.Rel(root, path)
		if err == nil && relative != ".." && !strings.HasPrefix(relative, "../") && !slices.Contains(files, path) {
			files = append(files, path)
		}
	}
	return files
}

func selectedConfigs(root string, used map[string]bool, values map[string]string) []string {
	switch {
	case used["--path"]:
		// The remove wrapper makes relative paths absolute before changing to
		// the setup directory. Open the path from the original cwd here.
		return []string{values["--path"]}
	case used["--base"]:
		return []string{filepath.Join(root, "config.toml")}
	case used["--profile"]:
		name := values["--profile"]
		if slices.Contains(profiles(root), name) {
			return []string{filepath.Join(root, "config."+name+".toml")}
		}
	default:
		physical, err := filepath.EvalSymlinks(root)
		if err == nil {
			if absolute, err := filepath.Abs(physical); err == nil {
				return activeConfigs(absolute)
			}
		}
	}
	return nil
}
