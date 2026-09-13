package ocsearch

import (
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"
)

// gitDirectoryName 是遍历时永远跳过的版本库目录名，与 ripgrep 一致
// （即使开启 --hidden 也不搜索 .git）。
const gitDirectoryName = ".git"

// walkConcurrencyLimit 限制同时展开的目录数。目录遍历是 I/O 密集的，并发度略高
// 于 CPU 数即可：再高只会放大小文件场景的随机寻道。
const walkConcurrencyLimit = 16

// walkOptions 是遍历与 glob 剪枝选项。
type walkOptions struct {
	cwd    string
	hidden bool
	globs  []globPattern
	// hasPathGlobs 表示存在含路径分隔符的 glob；只有这种模式需要维护相对路径段，
	// 其余（本项目全部调用都是 basename 模式）可以直接按条目名匹配。
	hasPathGlobs bool
}

// walker 在单次搜索中累计命中的文件绝对路径。
type walker struct {
	options   walkOptions
	deadline  time.Time
	semaphore chan struct{}
	waitGroup sync.WaitGroup
	guard     sync.Mutex
	files     []string
	firstErr  error
}

// childDirectory 是待并发遍历的子目录。
type childDirectory struct {
	path     string
	relative []string
}

// collectFiles 遍历 root（文件或目录），返回通过剪枝规则的绝对路径列表。
//
// 目录遍历并发进行，完成顺序不确定，因此最后统一排序，保证同一次调用的输出
// 稳定可复现（调用方本来也会自行排序）。
func collectFiles(root string, options walkOptions, deadline time.Time) ([]string, error) {
	info, err := os.Stat(root)
	if err != nil {
		return nil, fmt.Errorf("路径不可访问：%s：%w", root, err)
	}
	walker := &walker{
		options:   options,
		deadline:  deadline,
		semaphore: make(chan struct{}, walkConcurrencyLimit),
	}
	if !info.IsDir() {
		// 显式指定的文件不参与忽略规则，但仍受 glob 过滤。
		if walker.fileAccepted(filepath.Base(root), globSegments(options.cwd, root)) {
			walker.files = append(walker.files, root)
		}
		return walker.files, nil
	}
	walker.waitGroup.Add(1)
	walker.walkDirectory(root, globSegments(options.cwd, root), initialIgnoreNode(root))
	walker.waitGroup.Wait()
	if walker.firstErr != nil {
		return nil, walker.firstErr
	}
	sort.Strings(walker.files)
	return walker.files, nil
}

// walkDirectory 展开一个目录：忽略规则按目录继承，命中排除 glob 的目录直接
// 剪枝（如 node_modules、.omnicrawl），子目录交给并发分支继续。
//
// 信号量只包住目录列举与筛选，派生分支前必须释放，否则子目录会互相等待。
func (walker *walker) walkDirectory(dir string, relative []string, node *ignoreNode) {
	defer walker.waitGroup.Done()
	walker.semaphore <- struct{}{}
	entries, err := os.ReadDir(dir)
	if err != nil {
		// 不可读目录（权限不足、已被删除等）跳过，与 ripgrep 告警后继续一致。
		<-walker.semaphore
		return
	}
	// 忽略文件是否存在于本次目录列举的结果里，避免每个目录三次无果的 open。
	node = loadIgnoreNode(dir, presentIgnoreFiles(entries), node)

	var (
		local    []string
		children []childDirectory
	)
	for _, entry := range entries {
		if err := checkDeadline(walker.deadline); err != nil {
			walker.recordError(err)
			break
		}
		name := entry.Name()
		if name == gitDirectoryName {
			continue
		}
		if !walker.options.hidden && strings.HasPrefix(name, ".") {
			continue
		}
		kind := entry.Type()
		if kind&fs.ModeSymlink != 0 {
			// 与 ripgrep 默认一致：不跟随符号链接，避免循环遍历与越出搜索根。
			continue
		}
		isDirectory := entry.IsDir()
		if !isDirectory && !kind.IsRegular() {
			continue
		}
		var entryRelative []string
		if walker.options.hasPathGlobs {
			entryRelative = make([]string, 0, len(relative)+1)
			entryRelative = append(append(entryRelative, relative...), name)
		}
		if node.ignored(filepath.Join(dir, name), dir, name, isDirectory) {
			continue
		}
		if isDirectory {
			if !walker.directoryAccepted(name, entryRelative) {
				continue
			}
			children = append(children, childDirectory{
				path:     filepath.Join(dir, name),
				relative: entryRelative,
			})
			continue
		}
		if walker.fileAccepted(name, entryRelative) {
			local = append(local, filepath.Join(dir, name))
		}
	}
	<-walker.semaphore

	if len(local) > 0 {
		walker.guard.Lock()
		walker.files = append(walker.files, local...)
		walker.guard.Unlock()
	}
	for _, child := range children {
		walker.waitGroup.Add(1)
		go walker.walkDirectory(child.path, child.relative, node)
	}
}

// recordError 记录首个错误（超时等），只保留最早的一个。
func (walker *walker) recordError(err error) {
	walker.guard.Lock()
	defer walker.guard.Unlock()
	if walker.firstErr == nil {
		walker.firstErr = err
	}
}

// fileAccepted 判断文件是否通过 --glob 过滤：按出现顺序依次应用，后出现的
// 规则覆盖先出现的规则；没有 include 规则时默认接受。
func (walker *walker) fileAccepted(name string, relative []string) bool {
	if len(walker.options.globs) == 0 {
		return true
	}
	includeRules := 0
	accepted := false
	for _, glob := range walker.options.globs {
		if glob.negate {
			continue
		}
		includeRules++
		if glob.matchEntry(name, relative) {
			accepted = true
		}
	}
	if includeRules == 0 {
		accepted = true
	}
	for _, glob := range walker.options.globs {
		if glob.matchEntry(name, relative) {
			accepted = !glob.negate
		}
	}
	return accepted
}

// directoryAccepted 判断目录是否可继续遍历：只有排除规则能剪枝目录，
// include 规则（如 *.py）不应阻止进入子目录。
func (walker *walker) directoryAccepted(name string, relative []string) bool {
	for _, glob := range walker.options.globs {
		if glob.negate && glob.matchEntry(name, relative) {
			return false
		}
	}
	return true
}
