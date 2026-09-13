package ocsearch

import (
	"bufio"
	"bytes"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode/utf8"
)

// defaultTimeout 是调用方未指定超时时的兜底时限。
const defaultTimeout = 120 * time.Second

// binaryProbeSize 是二进制探测窗口：窗口内出现 NUL 字节即视为二进制并跳过，
// 与 ripgrep 默认行为一致（避免把压缩包、可执行文件内容灌进模型上下文）。
const binaryProbeSize = 8192

// scanWorkersLimit 限制并行扫描的 goroutine 上限，兼顾大仓库吞吐与内存占用。
const scanWorkersLimit = 16

// readerBufferSize 是单文件扫描的缓冲大小；缓冲通过 sync.Pool 复用，
// 避免每个文件都新申请一块（数十万小文件时是主要的 GC 压力来源）。
const readerBufferSize = 64 * 1024

// readerPool 复用 bufio.Reader（含其 64KB 缓冲）。
var readerPool = sync.Pool{
	New: func() any { return bufio.NewReaderSize(nil, readerBufferSize) },
}

// deadlineCheckInterval 是扫描长文件时检查超时的行间隔。
const deadlineCheckInterval = 4096

// ErrTimeout 表示搜索在给定超时时间内没有完成。
var ErrTimeout = errors.New("搜索超时")

// mode 是输出模式，对应 ripgrep 的 --json/--count/--files-with-matches/--files。
type mode int

const (
	modeMatches mode = iota
	modeCount
	modeFilesWithMatches
	modeFiles
)

// options 是一次搜索调用解析后的参数。
type options struct {
	mode         mode
	fixedStrings bool
	ignoreCase   bool
	hidden       bool
	maxCount     int
	pattern      string
	paths        []string
	globs        []globPattern
}

// matcher 是编译后的行匹配器。无正则时走子串/折叠子串查找，比正则引擎快；
// 有正则时先用必需字面量预筛，把不可能命中的行挡在引擎之外。
type matcher struct {
	expression    *regexp.Regexp
	literal       []byte
	foldedLiteral []byte
	prefilter     *literalFilter
}

// matchLine 判断一行内容（不含行尾终止符）是否命中。
func (matcher *matcher) matchLine(line []byte) bool {
	switch {
	case matcher.expression != nil:
		if matcher.prefilter != nil && !containsNeedle(line, matcher.prefilter) {
			return false
		}
		return matcher.expression.Match(line)
	case matcher.foldedLiteral != nil:
		return indexFoldASCII(line, matcher.foldedLiteral) >= 0
	default:
		return bytes.Contains(line, matcher.literal)
	}
}

// fileResult 是单个文件的扫描结果。
type fileResult struct {
	path  string
	count int
	lines []matchLine
}

// matchLine 是一条命中行；text 保留原始字节（含行尾终止符），
// --json 输出与非 UTF-8 占位都依赖它。
type matchLine struct {
	number int
	text   []byte
	offset int64
}

// Run 执行一次 rg 兼容的搜索调用，返回 (stdout, 退出码, error)。
// 退出码 0 表示有匹配，1 表示无匹配，2 表示执行失败（同时返回 error）。
func Run(argv []string, cwd string, timeout time.Duration) (string, int, error) {
	parsed, err := parseArgs(argv)
	if err != nil {
		return "", 2, err
	}
	if cwd == "" {
		cwd, err = os.Getwd()
		if err != nil {
			return "", 2, fmt.Errorf("获取当前目录失败：%w", err)
		}
	}
	if timeout <= 0 {
		timeout = defaultTimeout
	}
	deadline := time.Now().Add(timeout)

	// 先编译匹配器：无效正则在遍历文件前就失败，避免整棵目录树白扫一遍。
	var lineMatcher *matcher
	if parsed.mode != modeFiles {
		lineMatcher, err = buildMatcher(parsed)
		if err != nil {
			return "", 2, err
		}
	}

	roots := make([]string, 0, len(parsed.paths))
	for _, path := range parsed.paths {
		if !filepath.IsAbs(path) {
			path = filepath.Join(cwd, path)
		}
		roots = append(roots, filepath.Clean(path))
	}

	walking := walkOptions{
		cwd:          cwd,
		hidden:       parsed.hidden,
		globs:        parsed.globs,
		hasPathGlobs: hasPathGlobs(parsed.globs),
	}
	var files []string
	for _, root := range roots {
		collected, err := collectFiles(root, walking, deadline)
		if err != nil {
			return "", 2, err
		}
		files = append(files, collected...)
	}

	if parsed.mode == modeFiles {
		if len(files) == 0 {
			return "", 1, nil
		}
		var builder strings.Builder
		for _, file := range files {
			builder.WriteString(file)
			builder.WriteByte('\n')
		}
		return builder.String(), 0, nil
	}

	if len(files) == 0 {
		return "", 1, nil
	}
	results, err := scanFiles(files, lineMatcher, parsed, deadline)
	if err != nil {
		return "", 2, err
	}
	var builder strings.Builder
	if !render(results, parsed, &builder) {
		return "", 1, nil
	}
	return builder.String(), 0, nil
}

// parseArgs 解析项目调用到的 ripgrep 参数子集；未支持的参数直接报错，避免
// 静默忽略导致搜索结果与调用方预期不符。
func parseArgs(argv []string) (*options, error) {
	parsed := &options{mode: modeMatches}
	globCaseInsensitive := false
	var globSources []string
	index := 0
parse:
	for ; index < len(argv); index++ {
		argument := argv[index]
		if argument == "--" {
			index++
			break
		}
		switch argument {
		case "--json":
			parsed.mode = modeMatches
		case "--count":
			parsed.mode = modeCount
		case "--files-with-matches":
			parsed.mode = modeFilesWithMatches
		case "--files":
			parsed.mode = modeFiles
		case "--fixed-strings":
			parsed.fixedStrings = true
		case "--ignore-case":
			parsed.ignoreCase = true
		case "--hidden":
			parsed.hidden = true
		case "--no-require-git":
			// 忽略文件始终生效，不要求当前目录是 git 仓库。
		case "--glob-case-insensitive":
			globCaseInsensitive = true
		case "--line-number", "--with-filename", "--no-heading":
			// 输出来源固定带文件名与行号，无需额外处理。
		case "-m", "--max-count":
			if index+1 >= len(argv) {
				return nil, fmt.Errorf("参数 %s 缺少取值", argument)
			}
			index++
			count, err := strconv.Atoi(argv[index])
			if err != nil || count < 0 {
				return nil, fmt.Errorf("参数 %s 取值无效：%s", argument, argv[index])
			}
			parsed.maxCount = count
		case "--glob":
			if index+1 >= len(argv) {
				return nil, fmt.Errorf("参数 %s 缺少取值", argument)
			}
			index++
			globSources = append(globSources, argv[index])
		default:
			if strings.HasPrefix(argument, "-") {
				return nil, fmt.Errorf("不支持的搜索参数：%s", argument)
			}
			// 位置参数（pattern 与搜索路径）：其后不再解析参数，
			// 与 ripgrep 允许省略 '--' 的行为一致。
			break parse
		}
	}
	for _, source := range globSources {
		glob, err := compileGlob(source, globCaseInsensitive)
		if err != nil {
			return nil, err
		}
		parsed.globs = append(parsed.globs, glob)
	}
	rest := argv[index:]
	if parsed.mode == modeFiles {
		parsed.paths = rest
	} else {
		if len(rest) == 0 {
			return nil, errors.New("缺少搜索 pattern")
		}
		parsed.pattern = rest[0]
		parsed.paths = rest[1:]
	}
	if len(parsed.paths) == 0 {
		parsed.paths = []string{"."}
	}
	return parsed, nil
}

// hasPathGlobs 判断是否存在含路径分隔符的 glob 规则：只有这类规则需要维护
// 每个条目的相对路径段。
func hasPathGlobs(globs []globPattern) bool {
	for _, glob := range globs {
		if !glob.basenameOnly {
			return true
		}
	}
	return false
}

// buildMatcher 把 pattern 编译成行匹配器。
func buildMatcher(parsed *options) (*matcher, error) {
	if parsed.fixedStrings {
		if !parsed.ignoreCase {
			return &matcher{literal: []byte(parsed.pattern)}, nil
		}
		// 折叠安全的字面量直接用 ASCII 折叠搜索，不必进正则引擎。
		if needle, ok := foldSafeWholeLiteral(parsed.pattern); ok {
			return &matcher{foldedLiteral: needle}, nil
		}
		// 字面量含折叠到 ASCII 的 Unicode 字符（或非 ASCII）时退回正则，
		// 用正则实现 Unicode 感知的大小写不敏感匹配。
		compiled, err := regexp.Compile("(?i)" + regexp.QuoteMeta(parsed.pattern))
		if err != nil {
			return nil, fmt.Errorf("无效的搜索模式：%w", err)
		}
		return &matcher{expression: compiled}, nil
	}
	expression := parsed.pattern
	if parsed.ignoreCase {
		expression = "(?i)" + expression
	}
	compiled, err := regexp.Compile(expression)
	if err != nil {
		return nil, fmt.Errorf("无效的正则表达式：%w", err)
	}
	// 预筛字面量按原始模式抽取（(?i) 前缀只影响大小写，不影响字面量本身）。
	return &matcher{
		expression: compiled,
		prefilter:  extractLiteralFilter(parsed.pattern, parsed.ignoreCase),
	}, nil
}

// scanFiles 并行扫描文件，结果按输入顺序写回，保证输出稳定可复现。
func scanFiles(files []string, lineMatcher *matcher, parsed *options, deadline time.Time) ([]fileResult, error) {
	results := make([]fileResult, len(files))
	workers := runtime.NumCPU()
	if workers > scanWorkersLimit {
		workers = scanWorkersLimit
	}
	if workers < 1 {
		workers = 1
	}
	jobs := make(chan int)
	var waitGroup sync.WaitGroup
	var guard sync.Mutex
	var firstError error
	recordError := func(err error) {
		if err == nil {
			return
		}
		guard.Lock()
		defer guard.Unlock()
		if firstError == nil {
			firstError = err
		}
	}
	for worker := 0; worker < workers; worker++ {
		waitGroup.Add(1)
		go func() {
			defer waitGroup.Done()
			for index := range jobs {
				if err := checkDeadline(deadline); err != nil {
					recordError(err)
					continue
				}
				result, err := scanFile(files[index], lineMatcher, parsed, deadline)
				recordError(err)
				results[index] = result
			}
		}()
	}
	for index := range files {
		jobs <- index
	}
	close(jobs)
	waitGroup.Wait()
	if firstError != nil {
		return nil, firstError
	}
	return results, nil
}

// scanFile 逐行扫描一个文件；打不开或读取中途失败的文件跳过，与 ripgrep
// 告警后继续的语义一致。
func scanFile(path string, lineMatcher *matcher, parsed *options, deadline time.Time) (fileResult, error) {
	result := fileResult{path: path}
	file, err := os.Open(path)
	if err != nil {
		return result, nil
	}
	defer file.Close()

	reader := readerPool.Get().(*bufio.Reader)
	defer readerPool.Put(reader)
	reader.Reset(file)
	// 二进制探测直接复用缓冲读取的首块数据，避免额外一次 read + seek。
	probe, err := reader.Peek(binaryProbeSize)
	if err != nil && !errors.Is(err, io.EOF) && !errors.Is(err, bufio.ErrBufferFull) {
		return result, nil
	}
	if bytes.IndexByte(probe, 0) >= 0 {
		return result, nil
	}
	lineNumber := 0
	var offset int64
	for {
		line, readError := reader.ReadBytes('\n')
		if len(line) > 0 {
			lineNumber++
			if lineMatcher.matchLine(trimLineEnding(line)) {
				result.count++
				if parsed.mode == modeMatches {
					result.lines = append(result.lines, matchLine{
						number: lineNumber,
						text:   line,
						offset: offset,
					})
				}
				if parsed.maxCount > 0 && result.count >= parsed.maxCount {
					return result, nil
				}
			}
			offset += int64(len(line))
			if lineNumber%deadlineCheckInterval == 0 {
				if err := checkDeadline(deadline); err != nil {
					return result, err
				}
			}
		}
		if readError != nil {
			return result, nil
		}
	}
}

// render 按模式把扫描结果拼成 stdout；返回是否至少命中一个文件。
func render(results []fileResult, parsed *options, builder *strings.Builder) bool {
	matched := false
	switch parsed.mode {
	case modeCount:
		for _, result := range results {
			if result.count == 0 {
				continue
			}
			matched = true
			fmt.Fprintf(builder, "%s:%d\n", result.path, result.count)
		}
	case modeFilesWithMatches:
		for _, result := range results {
			if result.count == 0 {
				continue
			}
			matched = true
			builder.WriteString(result.path)
			builder.WriteByte('\n')
		}
	default:
		matched = renderJSON(results, builder)
	}
	return matched
}

// textValue 是 ripgrep --json 的行文本字段：合法 UTF-8 用 text，
// 否则用 base64 的 bytes，由调用方替换成占位说明。
type textValue struct {
	Text  string `json:"text,omitempty"`
	Bytes string `json:"bytes,omitempty"`
}

// pathValue 是 ripgrep --json 的路径字段。
type pathValue struct {
	Text string `json:"text"`
}

// recordData 是 ripgrep --json 的 data 字段子集。
type recordData struct {
	Path       pathValue  `json:"path"`
	Lines      *textValue `json:"lines,omitempty"`
	LineNumber int        `json:"line_number,omitempty"`
	Offset     *int64     `json:"absolute_offset,omitempty"`
}

// record 是一条 NDJSON 记录。
type record struct {
	Type string     `json:"type"`
	Data recordData `json:"data"`
}

// renderJSON 按 ripgrep --json 的 match 记录结构输出命中行：路径与行号都是
// 结构化字段，调用方无需再处理 "path:line:text" 的冒号歧义。
//
// 只输出 match 记录：ripgrep 还会为每个文件补 begin/end（以及末尾 summary）
// 记录，而调用方只消费 match，命中密集时这些记录能占到输出的一半。
func renderJSON(results []fileResult, builder *strings.Builder) bool {
	matched := false
	for _, result := range results {
		if result.count == 0 {
			continue
		}
		matched = true
		for _, line := range result.lines {
			offset := line.offset
			data := recordData{
				Path:       pathValue{Text: result.path},
				LineNumber: line.number,
				Offset:     &offset,
			}
			if utf8.Valid(line.text) {
				data.Lines = &textValue{Text: string(line.text)}
			} else {
				data.Lines = &textValue{Bytes: base64.StdEncoding.EncodeToString(line.text)}
			}
			writeRecord(builder, record{Type: "match", Data: data})
		}
	}
	return matched
}

// writeRecord 输出一条 NDJSON 记录。
func writeRecord(builder *strings.Builder, value record) {
	encoded, err := json.Marshal(value)
	if err != nil {
		return
	}
	builder.Write(encoded)
	builder.WriteByte('\n')
}

// trimLineEnding 去掉行尾的 '\n' 与 '\r'：ripgrep 的 '$' 允许出现在行尾
// 终止符之前，去掉终止符后 Go 正则的 '^'/'$' 才能给出同样的锚点行为。
func trimLineEnding(line []byte) []byte {
	if len(line) > 0 && line[len(line)-1] == '\n' {
		line = line[:len(line)-1]
	}
	if len(line) > 0 && line[len(line)-1] == '\r' {
		line = line[:len(line)-1]
	}
	return line
}

// checkDeadline 在超过截止时间后返回 ErrTimeout；零值表示不限制。
func checkDeadline(deadline time.Time) error {
	if deadline.IsZero() {
		return nil
	}
	if time.Now().After(deadline) {
		return ErrTimeout
	}
	return nil
}
