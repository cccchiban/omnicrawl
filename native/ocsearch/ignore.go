package ocsearch

import (
	"os"
	"path/filepath"
	"strings"
)

// ignoreFileNames 是遍历时读取的忽略文件，按优先级从低到高排列：同一目录内
// 后解析的规则覆盖先解析的规则，与 ripgrep/ignore crate 的优先级一致。
var ignoreFileNames = []string{".gitignore", ".ignore", ".rgignore"}

// ignoreNode 是目录继承链上的一层忽略规则；规则来自该目录下的忽略文件，
// 只作用于该目录及其子树。
type ignoreNode struct {
	dir    string
	parent *ignoreNode
	rules  []globPattern
}

// ignored 判断一个条目是否被忽略。越靠近目标的忽略文件优先级越高；同一文件
// 内后出现的规则覆盖先出现的规则；都没有命中时继续看祖先目录的规则。
//
// parentDir/name 用来避开最常见的 filepath.Rel：规则通常就来自条目所在目录，
// 此时相对路径段就是条目名本身。
func (node *ignoreNode) ignored(absPath string, parentDir string, name string, isDir bool) bool {
	for current := node; current != nil; current = current.parent {
		relative, ok := ignoreSegments(current.dir, parentDir, name, absPath)
		if !ok {
			continue
		}
		for index := len(current.rules) - 1; index >= 0; index-- {
			rule := current.rules[index]
			if rule.dirOnly && !isDir {
				continue
			}
			// 用 matchEntry（basename 规则直接比条目名）而不是 match(relative)：
			// 既走字面量快路径，也避免依赖 relative 的分段结果。
			if rule.matchEntry(name, relative) {
				return !rule.negate
			}
		}
	}
	return false
}

// ignoreSegments 返回条目相对规则目录的路径段。规则目录就是条目所在目录时
// 直接返回条目名，省掉一次 filepath.Rel。
func ignoreSegments(base string, parentDir string, name string, absPath string) ([]string, bool) {
	if base == parentDir {
		return []string{name}, true
	}
	return relativeSegments(base, absPath)
}

// presentIgnoreFiles 从目录列举结果里挑出实际存在的忽略文件，按优先级
// 从低到高返回；调用方因此不必对每个目录做三次无果的 open。
func presentIgnoreFiles(entries []os.DirEntry) []string {
	var present []string
	for _, candidate := range ignoreFileNames {
		for _, entry := range entries {
			if entry.Name() == candidate {
				present = append(present, candidate)
				break
			}
		}
	}
	return present
}

// loadIgnoreNode 读取目录下实际存在的忽略文件并挂到继承链上；目录内没有规则
// 时直接复用父节点，避免每个目录都分配一层。
func loadIgnoreNode(dir string, present []string, parent *ignoreNode) *ignoreNode {
	if len(present) == 0 {
		return parent
	}
	var rules []globPattern
	for _, name := range present {
		rules = append(rules, parseIgnoreFile(filepath.Join(dir, name))...)
	}
	if len(rules) == 0 {
		return parent
	}
	return &ignoreNode{dir: dir, parent: parent, rules: rules}
}

// initialIgnoreNode 构造搜索根之上的忽略规则链：与 ripgrep 一样读取父目录的
// 忽略文件，并在遇到 git 仓库根目录后停止向上收集。
func initialIgnoreNode(root string) *ignoreNode {
	ancestors := ancestorsOf(root)
	start := 0
	for index, dir := range ancestors {
		if hasGitDirectory(dir) {
			start = index
		}
	}
	var node *ignoreNode
	for _, dir := range ancestors[start:] {
		// 祖先目录数量很少，直接用 os.ReadDir 结果判断忽略文件是否存在。
		entries, err := os.ReadDir(dir)
		if err != nil {
			continue
		}
		node = loadIgnoreNode(dir, presentIgnoreFiles(entries), node)
	}
	return node
}

// ancestorsOf 返回目录的所有祖先，顺序由外层到内层（不含目录自身）。
func ancestorsOf(dir string) []string {
	var ancestors []string
	parent := filepath.Dir(dir)
	for parent != dir {
		ancestors = append(ancestors, parent)
		next := filepath.Dir(parent)
		if next == parent {
			break
		}
		parent = next
	}
	for left, right := 0, len(ancestors)-1; left < right; left, right = left+1, right-1 {
		ancestors[left], ancestors[right] = ancestors[right], ancestors[left]
	}
	return ancestors
}

// hasGitDirectory 判断目录是否为 git 仓库根目录。
func hasGitDirectory(dir string) bool {
	info, err := os.Stat(filepath.Join(dir, ".git"))
	return err == nil && info.IsDir()
}

// parseIgnoreFile 解析一个忽略文件；文件不存在或不可读时返回空规则。
func parseIgnoreFile(path string) []globPattern {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil
	}
	text := strings.ReplaceAll(string(data), "\r\n", "\n")
	var patterns []globPattern
	for _, raw := range strings.Split(text, "\n") {
		// 行尾未转义的空格无意义；转义空格（'\ '）保留。
		line := strings.TrimRight(raw, " \t")
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		pattern, err := compileGlob(line, false)
		if err != nil {
			continue
		}
		patterns = append(patterns, pattern)
	}
	return patterns
}

// relativeSegments 返回 target 相对 base 的路径段；target 不在 base 之下时
// 返回 ok=false。忽略规则依赖这一严格语义：祖先目录的规则只作用于子树。
func relativeSegments(base string, target string) ([]string, bool) {
	relative, err := filepath.Rel(base, target)
	if err != nil {
		return nil, false
	}
	// filepath.Rel 在 Windows 上返回反斜杠分隔（如 "src\app.py"），而 glob /
	// 忽略规则统一按 '/' 分段；不转换会把整条路径当成单个路径段，导致祖先目录
	// 的规则在子目录中全部失配。
	return splitSlashedPath(filepath.ToSlash(relative))
}

// globSegments 返回 glob 匹配用的路径段：目标在 base 之下时用相对段，
// 否则退回绝对路径段，使 basename 与 '**/' 模式在搜索根之外仍然生效。
func globSegments(base string, target string) []string {
	if relative, ok := relativeSegments(base, target); ok {
		return relative
	}
	segments, _ := splitSlashedPath(filepath.ToSlash(strings.TrimPrefix(target, filepath.VolumeName(target))))
	return segments
}

// splitSlashedPath 把 '/'-分隔的相对路径切成路径段；'.' 返回空段，
// 以 '..' 开头表示不在基准目录之下。
func splitSlashedPath(slashed string) ([]string, bool) {
	if slashed == "." {
		return nil, true
	}
	if slashed == ".." || strings.HasPrefix(slashed, "../") {
		return nil, false
	}
	trimmed := strings.Trim(slashed, "/")
	if trimmed == "" {
		return nil, true
	}
	return strings.Split(trimmed, "/"), true
}
