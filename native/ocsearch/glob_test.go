package ocsearch

import "testing"

func TestGlobMatches(t *testing.T) {
	tests := []struct {
		name     string
		pattern  string
		path     []string
		expected bool
	}{
		{"basename 匹配任意层级", "*.py", []string{"src", "main.py"}, true},
		{"basename 不跨目录", "*.py", []string{"main.py", "nested"}, false},
		{"basename 命中根文件", "*.py", []string{"main.py"}, true},
		{"保护路径 glob 命中目录", "**/node_modules", []string{"web", "node_modules"}, true},
		{"保护路径 glob 命中根目录", "**/node_modules", []string{"node_modules"}, true},
		{"保护路径 glob 未命中", "**/node_modules", []string{"web", "src"}, false},
		{"点文件前缀变体", "**/.env.*", []string{"app", ".env.local"}, true},
		{"锚定模式只匹配根", "/build", []string{"a", "build"}, false},
		{"锚定模式命中根", "/build", []string{"build"}, true},
		{"中间 ** 跨段", "a/**/b", []string{"a", "b"}, true},
		{"中间 ** 跨多段", "a/**/b", []string{"a", "x", "y", "b"}, true},
		{"尾部 ** 匹配子内容", "build/**", []string{"build", "out.txt"}, true},
		{"尾部 ** 不匹配目录自身", "build/**", []string{"build"}, false},
		{"字符集", "file[0-9].txt", []string{"file3.txt"}, true},
		{"字符集取反", "file[!0-9].txt", []string{"file3.txt"}, false},
		{"单字符通配", "a?c.go", []string{"abc.go"}, true},
		{"星号不跨目录段", "a/*/c.go", []string{"a", "b", "c.go"}, true},
		// 中部斜杠锚定到搜索根（gitignore/ripgrep 语义），前导 '**/' 才表示任意层级。
		{"中段斜杠锚定命中", "src/*.py", []string{"src", "a.py"}, true},
		{"中段斜杠不跨层级", "src/*.py", []string{"x", "src", "a.py"}, false},
		{"前导 globstar 保持任意层级", "**/src/*.py", []string{"x", "src", "a.py"}, true},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			pattern, err := compileGlob(test.pattern, false)
			if err != nil {
				t.Fatalf("编译 %q 失败：%v", test.pattern, err)
			}
			if got := pattern.match(test.path); got != test.expected {
				t.Errorf("%q 匹配 %v = %v，期望 %v", test.pattern, test.path, got, test.expected)
			}
		})
	}
}

func TestGlobNegationAndCaseInsensitivity(t *testing.T) {
	pattern, err := compileGlob("!skip*", true)
	if err != nil {
		t.Fatalf("编译失败：%v", err)
	}
	if !pattern.negate {
		t.Error("! 前缀应标记为排除规则")
	}
	if !pattern.match([]string{"SKIPME.py"}) {
		t.Error("大小写不敏感模式下应命中大写文件名")
	}

	directory, err := compileGlob("node_modules/", false)
	if err != nil {
		t.Fatalf("编译失败：%v", err)
	}
	if !directory.dirOnly {
		t.Error("结尾 / 应标记为只匹配目录")
	}
}

func TestCompileGlobRejectsEmptyPattern(t *testing.T) {
	if _, err := compileGlob("!", false); err == nil {
		t.Error("空模式应当返回错误")
	}
}
