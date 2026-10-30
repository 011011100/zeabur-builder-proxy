// This test is compiled in the pinned upstream Railpack core/mise package.
package mise

import (
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

func TestMingxingProxyEnvironmentReachesMiseSubprocess(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("the maintained planner is a Linux image; this probe executes a POSIX shell")
	}
	values := map[string]string{
		"http_proxy":  "http://proxy.example.test:3128",
		"HTTP_PROXY":  "http://proxy.example.test:3128",
		"https_proxy": "http://proxy.example.test:3128",
		"HTTPS_PROXY": "http://proxy.example.test:3128",
		"no_proxy":    "localhost,.svc,10.43.0.0/16",
		"NO_PROXY":    "localhost,.svc,10.43.0.0/16",
	}
	for name, value := range values {
		t.Setenv(name, value)
	}
	t.Setenv("UNRELATED_PARENT_CREDENTIAL", "must-not-reach-mise")
	directory := t.TempDir()
	binary := filepath.Join(directory, "mise-child")
	if err := os.WriteFile(binary, []byte("#!/bin/sh\nexec env\n"), 0755); err != nil {
		t.Fatal(err)
	}
	m := &Mise{binaryPath: binary, cacheDir: directory}
	check := func() {
		t.Helper()
		output, err := m.runCmdWithEnv([]string{"MISE_NO_CONFIG=1"}, "latest", "node@lts")
		if err != nil {
			t.Fatal(err)
		}
		observed := make(map[string]string)
		for _, line := range strings.Split(strings.TrimSpace(output), "\n") {
			if name, value, ok := strings.Cut(line, "="); ok {
				observed[name] = value
			}
		}
		for name, wanted := range values {
			if value, exists := observed[name]; !exists || value != wanted {
				t.Errorf("Mise subprocess did not receive proxy variable %s", name)
			}
		}
		if _, exists := observed["UNRELATED_PARENT_CREDENTIAL"]; exists {
			t.Error("Mise subprocess inherited unrelated parent credentials")
		}
		if observed["HOME"] != directory || observed["MISE_NO_CONFIG"] != "1" {
			t.Error("proxy support changed the existing Mise sandbox environment")
		}
	}
	check()
	// An explicitly empty value is a setting, not a missing variable.
	t.Setenv("http_proxy", "")
	values["http_proxy"] = ""
	check()
}
