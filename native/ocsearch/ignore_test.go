package ocsearch

import (
	"os"
	"path/filepath"
	"testing"
)

func TestIgnoreNodeMatchesDirectoryScopedRules(t *testing.T) {
	root := t.TempDir()
	writeTestFile(t, filepath.Join(root, ".gitignore"), "node_modules/\n*.log\n!keep.log\nsecret.txt\nvendor/lib/\ndocs/**\n")
	writeTestFile(t, filepath.Join(root, "src", ".gitignore"), "generated/\n")

	node := loadNodeForTest(t, root, initialIgnoreNode(root))
	srcNode := loadNodeForTest(t, filepath.Join(root, "src"), node)

	tests := []struct {
		name     string
		node     *ignoreNode
		path     string
		isDir    bool
		expected bool
	}{
		{"目录规则剪枝", node, filepath.Join(root, "node_modules"), true, true},
		{"后缀规则命中文件", node, filepath.Join(root, "app.log"), false, true},
		{"否定规则重新包含", node, filepath.Join(root, "keep.log"), false, false},
		{"普通文件不忽略", node, filepath.Join(root, "main.py"), false, false},
		{"子目录规则只作用于子树", srcNode, filepath.Join(root, "src", "generated"), true, true},
		{"子目录规则不影响同级", node, filepath.Join(root, "generated"), true, false},
		{"父规则在子树仍生效", srcNode, filepath.Join(root, "src", "app.log"), false, true},
		// 回归：祖先目录的 basename 规则必须作用于深层子目录（Windows 上 filepath.Rel
		// 的反斜杠分段曾使这些规则全部失配）。
		{"父 basename 规则作用于子树", node, filepath.Join(root, "src", "secret.txt"), false, true},
		{"父 basename 目录规则作用于子树", node, filepath.Join(root, "src", "node_modules"), true, true},
		// 中部斜杠规则相对规则目录锚定，不按任意层级后缀匹配。
		{"中段斜杠规则锚定命中", node, filepath.Join(root, "vendor", "lib"), true, true},
		{"中段斜杠规则不跨层级", node, filepath.Join(root, "src", "vendor", "lib"), true, false},
		{"尾部 globstar 作用于子树", node, filepath.Join(root, "docs", "guide", "a.md"), false, true},
		{"尾部 globstar 不跨层级", node, filepath.Join(root, "src", "docs", "a.md"), false, false},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			parentDir := filepath.Dir(test.path)
			if got := test.node.ignored(test.path, parentDir, filepath.Base(test.path), test.isDir); got != test.expected {
				t.Errorf("ignored(%s) = %v，期望 %v", test.path, got, test.expected)
			}
		})
	}
}

// loadNodeForTest 按生产路径读取目录下的忽略文件（先列举再读取实际存在的）。
func loadNodeForTest(t *testing.T, dir string, parent *ignoreNode) *ignoreNode {
	t.Helper()
	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatalf("读取目录失败：%v", err)
	}
	return loadIgnoreNode(dir, presentIgnoreFiles(entries), parent)
}

func writeTestFile(t *testing.T, path string, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatalf("创建目录失败：%v", err)
	}
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatalf("写入文件失败：%v", err)
	}
}
