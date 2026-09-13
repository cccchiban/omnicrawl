package ocsearch

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"
	"time"
)

type parsedMatch struct {
	path     string
	line     int
	text     string
	hasBytes bool
}

func runForTest(t *testing.T, argv []string) (string, int) {
	t.Helper()
	stdout, code, err := Run(argv, "", 30*time.Second)
	if err != nil {
		t.Fatalf("Run(%v) 失败：%v", argv, err)
	}
	return stdout, code
}

func parseMatches(t *testing.T, stdout string) []parsedMatch {
	t.Helper()
	var matches []parsedMatch
	for _, line := range strings.Split(stdout, "\n") {
		if strings.TrimSpace(line) == "" {
			continue
		}
		var record struct {
			Type string `json:"type"`
			Data struct {
				Path struct {
					Text string `json:"text"`
				} `json:"path"`
				LineNumber int `json:"line_number"`
				Lines      struct {
					Text  string `json:"text"`
					Bytes string `json:"bytes"`
				} `json:"lines"`
			} `json:"data"`
		}
		if err := json.Unmarshal([]byte(line), &record); err != nil {
			t.Fatalf("解析 NDJSON 失败：%v（%s）", err, line)
		}
		if record.Type != "match" {
			continue
		}
		matches = append(matches, parsedMatch{
			path:     filepath.Base(record.Data.Path.Text),
			line:     record.Data.LineNumber,
			text:     strings.TrimRight(record.Data.Lines.Text, "\r\n"),
			hasBytes: record.Data.Lines.Bytes != "",
		})
	}
	return matches
}

func jsonArgs(root string, pattern string) []string {
	return []string{"--json", "--line-number", "--hidden", "--no-require-git", "--", pattern, root}
}

func TestRunJSONReportsMatchesWithLineNumbers(t *testing.T) {
	root := t.TempDir()
	writeTestFile(t, filepath.Join(root, "a.txt"), "hello world\n")
	writeTestFile(t, filepath.Join(root, "nested", "b.txt"), "hello again\n")
	writeTestFile(t, filepath.Join(root, "c.txt"), "unrelated\n")

	stdout, code := runForTest(t, jsonArgs(root, "hello"))
	if code != 0 {
		t.Fatalf("有匹配时退出码应为 0，实际 %d", code)
	}
	matches := parseMatches(t, stdout)
	if len(matches) != 2 {
		t.Fatalf("期望 2 条匹配，实际 %d（%v）", len(matches), matches)
	}
	if matches[0].path != "a.txt" || matches[0].line != 1 || matches[0].text != "hello world" {
		t.Errorf("首条匹配不符合预期：%+v", matches[0])
	}
	if matches[1].path != "b.txt" {
		t.Errorf("次条匹配文件错误：%+v", matches[1])
	}
}

func TestRunCaseSensitivity(t *testing.T) {
	root := t.TempDir()
	writeTestFile(t, filepath.Join(root, "a.txt"), "Hello\nhello\n")

	sensitiveArgs := []string{"--json", "--no-require-git", "--", "hello", root}
	stdout, _ := runForTest(t, sensitiveArgs)
	if matches := parseMatches(t, stdout); len(matches) != 1 || matches[0].line != 2 {
		t.Fatalf("区分大小写时只应命中第 2 行：%v", matches)
	}

	insensitiveArgs := []string{"--json", "--ignore-case", "--no-require-git", "--", "hello", root}
	stdout, _ = runForTest(t, insensitiveArgs)
	if matches := parseMatches(t, stdout); len(matches) != 2 {
		t.Fatalf("忽略大小写时应命中 2 行：%v", matches)
	}
}

func TestRunRegexAndFixedStringSemantics(t *testing.T) {
	root := t.TempDir()
	writeTestFile(t, filepath.Join(root, "a.txt"), "axb\na.*b\n")

	stdout, _ := runForTest(t, jsonArgs(root, "a.b"))
	if matches := parseMatches(t, stdout); len(matches) != 1 || matches[0].text != "axb" {
		t.Fatalf("默认正则语义应只命中 axb：%v", matches)
	}

	fixedArgs := []string{"--json", "--fixed-strings", "--no-require-git", "--", "a.*b", root}
	stdout, _ = runForTest(t, fixedArgs)
	if matches := parseMatches(t, stdout); len(matches) != 1 || matches[0].text != "a.*b" {
		t.Fatalf("字面量语义应只命中 a.*b：%v", matches)
	}
}

func TestRunAnchorsMatchLineRatherThanWholeFile(t *testing.T) {
	root := t.TempDir()
	writeTestFile(t, filepath.Join(root, "a.txt"), "foo\nbar\nfoobar\n")

	stdout, _ := runForTest(t, jsonArgs(root, "^foo$"))
	matches := parseMatches(t, stdout)
	if len(matches) != 1 || matches[0].line != 1 {
		t.Fatalf("^foo$ 应只命中第 1 行：%v", matches)
	}
}

func TestRunCountAndFilesWithMatches(t *testing.T) {
	root := t.TempDir()
	writeTestFile(t, filepath.Join(root, "a.txt"), "x\nx\n")
	writeTestFile(t, filepath.Join(root, "b.txt"), "y\n")

	base := []string{"--hidden", "--no-require-git", "--", "x", root}

	countArgs := append([]string{"--count", "--with-filename"}, base...)
	stdout, _ := runForTest(t, countArgs)
	if strings.TrimSpace(stdout) != filepath.Join(root, "a.txt")+":2" {
		t.Fatalf("计数输出不符合预期：%q", stdout)
	}

	fileArgs := append([]string{"--files-with-matches", "-m", "1"}, base...)
	stdout, _ = runForTest(t, fileArgs)
	if strings.TrimSpace(stdout) != filepath.Join(root, "a.txt") {
		t.Fatalf("仅列文件输出不符合预期：%q", stdout)
	}
}

func TestRunNoMatchReturnsExitCodeOne(t *testing.T) {
	root := t.TempDir()
	writeTestFile(t, filepath.Join(root, "a.txt"), "hello\n")

	stdout, code := runForTest(t, jsonArgs(root, "missing"))
	if code != 1 || stdout != "" {
		t.Fatalf("无匹配时应返回空输出与退出码 1，实际 code=%d stdout=%q", code, stdout)
	}
}

func TestRunMaxCountCapsMatchesPerFile(t *testing.T) {
	root := t.TempDir()
	writeTestFile(t, filepath.Join(root, "a.txt"), "l1\nl2\nl3\n")

	args := append([]string{"--json", "-m", "2", "--no-require-git"}, "--", "l", root)
	stdout, _ := runForTest(t, args)
	if matches := parseMatches(t, stdout); len(matches) != 2 {
		t.Fatalf("每文件最多 2 条匹配，实际 %d", len(matches))
	}
}

func TestRunFilesHonoursGitIgnoreAndHiddenFiles(t *testing.T) {
	root := t.TempDir()
	writeTestFile(t, filepath.Join(root, ".gitignore"), "node_modules/\nbuild/\n")
	writeTestFile(t, filepath.Join(root, "src", "main.py"), "def main(): pass\n")
	writeTestFile(t, filepath.Join(root, "node_modules", "big.js"), "const x = 1;\n")
	writeTestFile(t, filepath.Join(root, "build", "out.txt"), "artifact\n")
	writeTestFile(t, filepath.Join(root, ".github", "workflow.yml"), "on: push\n")

	stdout, code := runForTest(t, []string{"--files", "--hidden", "--no-require-git", root})
	if code != 0 {
		t.Fatalf("列出文件时退出码应为 0，实际 %d", code)
	}
	if strings.Contains(stdout, "node_modules") || strings.Contains(stdout, "build") {
		t.Errorf("被 .gitignore 忽略的目录不应出现：%q", stdout)
	}
	if !strings.Contains(stdout, filepath.Join(".github", "workflow.yml")) {
		t.Errorf("--hidden 应包含隐藏目录：%q", stdout)
	}
	if !strings.Contains(stdout, filepath.Join("src", "main.py")) {
		t.Errorf("普通文件应被列出：%q", stdout)
	}
	if strings.Contains(stdout, ".gitignore") == false {
		t.Errorf("--hidden 应包含隐藏文件：%q", stdout)
	}
}

func TestRunParallelWalkIsDeterministicAndPruned(t *testing.T) {
	root := t.TempDir()
	writeTestFile(t, filepath.Join(root, ".gitignore"), "skip/\n")
	for index := 0; index < 40; index++ {
		writeTestFile(t, filepath.Join(root, fmt.Sprintf("pkg%02d", index), "main.go"), "package main\n")
		writeTestFile(t, filepath.Join(root, "skip", fmt.Sprintf("s%02d", index), "bad.go"), "package skip\n")
	}

	args := []string{"--files", "--hidden", "--no-require-git", root}
	first, code := runForTest(t, args)
	if code != 0 {
		t.Fatalf("列出文件时退出码应为 0，实际 %d", code)
	}
	// 并发遍历的完成顺序不定，输出必须仍然稳定且按字典序排列。
	for attempt := 0; attempt < 5; attempt++ {
		again, _ := runForTest(t, args)
		if again != first {
			t.Fatalf("第 %d 次遍历输出与首次不一致", attempt+1)
		}
	}
	if strings.Contains(first, "bad.go") {
		t.Errorf("被 .gitignore 忽略的目录未被剪枝：%q", first)
	}
	lines := strings.Split(strings.TrimRight(first, "\n"), "\n")
	if len(lines) != 41 {
		t.Fatalf("期望 40 个文件 + 1 个 .gitignore，实际 %d 个", len(lines))
	}
	if !sort.StringsAreSorted(lines) {
		t.Errorf("输出未按字典序排列：%v", lines[:3])
	}
}

func TestRunGlobPrunesProtectedDirectories(t *testing.T) {
	root := t.TempDir()
	writeTestFile(t, filepath.Join(root, "node_modules", "big.js"), "const x = 1;\n")
	writeTestFile(t, filepath.Join(root, ".env.local"), "TOKEN=1\n")
	writeTestFile(t, filepath.Join(root, "src", "main.py"), "x = 1\n")

	args := []string{"--files", "--hidden", "--no-require-git", "--glob", "!**/node_modules", "--glob", "!**/.env.*", root}
	stdout, _ := runForTest(t, args)
	if strings.Contains(stdout, "node_modules") || strings.Contains(stdout, ".env.local") {
		t.Fatalf("保护路径 glob 未剪枝：%q", stdout)
	}
	if !strings.Contains(stdout, filepath.Join("src", "main.py")) {
		t.Fatalf("普通文件应保留：%q", stdout)
	}
}

func TestRunIncludeGlobKeepsMatchingFilesOnly(t *testing.T) {
	root := t.TempDir()
	writeTestFile(t, filepath.Join(root, "a.py"), "keyword\n")
	writeTestFile(t, filepath.Join(root, "b.txt"), "keyword\n")
	writeTestFile(t, filepath.Join(root, "skip.py"), "keyword\n")

	args := []string{
		"--json", "--hidden", "--no-require-git", "--glob-case-insensitive",
		"--glob", "*.py", "--glob", "!skip*", "--", "keyword", root,
	}
	stdout, _ := runForTest(t, args)
	matches := parseMatches(t, stdout)
	if len(matches) != 1 || matches[0].path != "a.py" {
		t.Fatalf("include/exclude glob 结果不符合预期：%v", matches)
	}
}

func TestRunNonUTF8LineFallsBackToBytes(t *testing.T) {
	root := t.TempDir()
	content := append([]byte("match \xff\xfe line\n"), []byte("clean match\n")...)
	if err := os.WriteFile(filepath.Join(root, "raw.txt"), content, 0o644); err != nil {
		t.Fatalf("写入文件失败：%v", err)
	}

	stdout, _ := runForTest(t, jsonArgs(root, "match"))
	matches := parseMatches(t, stdout)
	if len(matches) != 2 {
		t.Fatalf("应命中 2 行：%v", matches)
	}
	if !matches[0].hasBytes {
		t.Error("非 UTF-8 行应通过 bytes 字段返回")
	}
	if matches[1].hasBytes {
		t.Error("合法 UTF-8 行应通过 text 字段返回")
	}
}

func TestRunSkipsBinaryFiles(t *testing.T) {
	root := t.TempDir()
	binary := append([]byte("match\x00binary\n"), []byte("match text\n")...)
	if err := os.WriteFile(filepath.Join(root, "blob.bin"), binary, 0o644); err != nil {
		t.Fatalf("写入文件失败：%v", err)
	}

	stdout, code := runForTest(t, jsonArgs(root, "match"))
	if code != 1 {
		t.Fatalf("二进制文件应被跳过，实际 code=%d stdout=%q", code, stdout)
	}
}

func TestRunRejectsUnsupportedArguments(t *testing.T) {
	root := t.TempDir()
	writeTestFile(t, filepath.Join(root, "a.txt"), "hello\n")

	if _, code, err := Run([]string{"--unsupported-flag", "--", "hello", root}, "", time.Second); err == nil || code != 2 {
		t.Fatalf("不支持的参数应返回退出码 2 与错误，实际 code=%d err=%v", code, err)
	}
}

func TestRunInvalidRegexReturnsError(t *testing.T) {
	root := t.TempDir()
	writeTestFile(t, filepath.Join(root, "a.txt"), "hello\n")

	if _, code, err := Run(jsonArgs(root, "(unclosed"), "", time.Second); err == nil || code != 2 {
		t.Fatalf("非法正则应返回退出码 2 与错误，实际 code=%d err=%v", code, err)
	}
}

func TestRunMissingPathReturnsError(t *testing.T) {
	root := filepath.Join(t.TempDir(), "missing")

	if _, code, err := Run(jsonArgs(root, "hello"), "", time.Second); err == nil || code != 2 {
		t.Fatalf("路径不存在应返回退出码 2 与错误，实际 code=%d err=%v", code, err)
	}
}

func TestRunHonoursDeadline(t *testing.T) {
	root := t.TempDir()
	writeTestFile(t, filepath.Join(root, "a.txt"), strings.Repeat("needle\n", 200000))

	if _, code, err := Run(jsonArgs(root, "needle"), "", time.Nanosecond); err == nil || code != 2 {
		t.Fatalf("超时应返回退出码 2 与错误，实际 code=%d err=%v", code, err)
	}
}
