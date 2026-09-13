package ocsearch

import (
	"fmt"
	"strings"
)

// globPattern 是编译后的 glob 模式，覆盖 gitignore 与 ripgrep --glob 共用的
// 语法子集：
//
//   - '**' 跨路径段匹配（可匹配零个或多个段）
//   - '*' 与 '?' 在单个路径段内通配
//   - '[...]' 字符集，支持 '!'/'^' 取反与 'a-z' 区间
//   - '\' 转义下一个字符
//   - 前缀 '!' 表示排除规则（negate 为 true）
//   - 结尾 '/' 表示只匹配目录（dirOnly 为 true）
//   - 前缀或中部 '/' 表示锚定到规则所在目录（anchored 为 true）；只有前导
//     '**/' 才保持"任意层级"的非锚定匹配
type globPattern struct {
	source          string
	segments        []string
	negate          bool
	dirOnly         bool
	anchored        bool
	caseInsensitive bool
	// basenameOnly 表示模式只有一个路径段（无 '/'），可以直接拿条目名匹配，
	// 不必计算相对路径段。
	basenameOnly bool
	// literal 是 basenameOnly 且不含通配符时的字面量（已按大小写归一），
	// 命中判断退化成一次字符串比较：遍历热路径上绝大多数规则都是这种。
	literal string
}

// compileGlob 编译一条 glob 模式。
func compileGlob(source string, caseInsensitive bool) (globPattern, error) {
	pattern := globPattern{source: source, caseInsensitive: caseInsensitive}
	text := source
	if strings.HasPrefix(text, "!") {
		pattern.negate = true
		text = strings.TrimPrefix(text, "!")
	}
	if strings.HasSuffix(text, "/") {
		pattern.dirOnly = true
		text = strings.TrimRight(text, "/")
	}
	if strings.HasPrefix(text, "/") {
		pattern.anchored = true
		text = strings.TrimPrefix(text, "/")
	}
	// 前导 '**/' 表示"任意层级"，与不含 '/' 的 basename 模式一样按路径后缀匹配，
	// 因此直接剥离后走非锚定分支；显式写出才保留非锚定语义。
	anyDepth := strings.HasPrefix(text, "**/")
	text = strings.TrimPrefix(text, "**/")
	if text == "" {
		return globPattern{}, fmt.Errorf("glob 模式无效：%q", source)
	}
	if text == "**" {
		pattern.segments = []string{"**"}
		return pattern, nil
	}
	segments := make([]string, 0, 4)
	for _, segment := range strings.Split(text, "/") {
		if segment == "" {
			continue
		}
		segments = append(segments, segment)
	}
	if len(segments) == 0 {
		return globPattern{}, fmt.Errorf("glob 模式无效：%q", source)
	}
	// gitignore 的 'a/**' 只匹配 a 之下的内容，不含 a 本身；展开成 'a/*/**'。
	if len(segments) > 1 && segments[len(segments)-1] == "**" {
		segments = append(segments[:len(segments)-1], "*", "**")
	}
	// gitignore/ripgrep 语义：模式中部含 '/' 时相对规则目录（或搜索根）锚定，
	// 不能按任意层级的后缀匹配；只有显式的前导 '**/' 才表示任意层级。
	if !pattern.anchored && !anyDepth && len(segments) > 1 {
		pattern.anchored = true
	}
	pattern.segments = segments
	pattern.basenameOnly = !pattern.anchored && len(segments) == 1
	if pattern.basenameOnly && !hasGlobMeta(segments[0]) {
		pattern.literal = lowerIf(segments[0], caseInsensitive)
	}
	return pattern, nil
}

// hasGlobMeta 判断模式段是否含通配符。
func hasGlobMeta(segment string) bool {
	return strings.ContainsAny(segment, "*?[")
}

// lowerIf 按需做大小写归一。
func lowerIf(text string, caseInsensitive bool) string {
	if caseInsensitive {
		return strings.ToLower(text)
	}
	return text
}

// matchEntry 判断一个遍历条目是否命中该模式。名字（basename）是热路径上
// 唯一必需的信息：只有含路径分隔符的模式才需要完整相对路径段。
func (pattern globPattern) matchEntry(name string, relative []string) bool {
	if pattern.literal != "" {
		return lowerIf(name, pattern.caseInsensitive) == pattern.literal
	}
	if pattern.basenameOnly {
		return segmentMatch(pattern.segments[0], name, pattern.caseInsensitive)
	}
	return pattern.match(relative)
}

// match 判断路径段序列是否命中该模式。非锚定模式（basename 语义）允许出现在
// 任意层级，因此对路径的每个后缀各试一次。
func (pattern globPattern) match(path []string) bool {
	if len(pattern.segments) == 0 {
		return false
	}
	if pattern.anchored {
		return matchSegments(pattern.segments, path, pattern.caseInsensitive)
	}
	for start := 0; start <= len(path); start++ {
		if matchSegments(pattern.segments, path[start:], pattern.caseInsensitive) {
			return true
		}
	}
	return false
}

// matchSegments 按路径段递归匹配，'**' 可消耗零个或多个路径段。
func matchSegments(patterns []string, path []string, caseInsensitive bool) bool {
	if len(patterns) == 0 {
		return len(path) == 0
	}
	if patterns[0] == "**" {
		for consumed := 0; consumed <= len(path); consumed++ {
			if matchSegments(patterns[1:], path[consumed:], caseInsensitive) {
				return true
			}
		}
		return false
	}
	if len(path) == 0 {
		return false
	}
	if !segmentMatch(patterns[0], path[0], caseInsensitive) {
		return false
	}
	return matchSegments(patterns[1:], path[1:], caseInsensitive)
}

// segmentMatch 在单个路径段内匹配，'*' 与 '?' 不跨 '/'。
func segmentMatch(pattern string, name string, caseInsensitive bool) bool {
	if caseInsensitive {
		pattern = strings.ToLower(pattern)
		name = strings.ToLower(name)
	}
	return matchSegmentRunes([]rune(pattern), []rune(name))
}

// matchSegmentRunes 用双指针 + 星号回溯实现段内 glob 匹配。
func matchSegmentRunes(pattern []rune, name []rune) bool {
	patternIndex, nameIndex := 0, 0
	starPattern, starName := -1, 0
	for nameIndex < len(name) {
		if patternIndex < len(pattern) {
			switch pattern[patternIndex] {
			case '*':
				starPattern, starName = patternIndex, nameIndex
				patternIndex++
				continue
			case '?':
				patternIndex++
				nameIndex++
				continue
			case '[':
				matched, next := matchClass(pattern, patternIndex, name[nameIndex])
				if matched {
					patternIndex = next
					nameIndex++
					continue
				}
			case '\\':
				if patternIndex+1 < len(pattern) && pattern[patternIndex+1] == name[nameIndex] {
					patternIndex += 2
					nameIndex++
					continue
				}
			default:
				if pattern[patternIndex] == name[nameIndex] {
					patternIndex++
					nameIndex++
					continue
				}
			}
		}
		if starPattern < 0 {
			return false
		}
		// 回溯：让最近的 '*' 多消耗一个字符。
		starName++
		nameIndex = starName
		patternIndex = starPattern + 1
	}
	for patternIndex < len(pattern) && pattern[patternIndex] == '*' {
		patternIndex++
	}
	return patternIndex == len(pattern)
}

// matchClass 判断字符是否命中 pattern[start] 处的 '[' 字符集，返回是否命中与
// 字符集结束后的模式下标。
func matchClass(pattern []rune, start int, character rune) (bool, int) {
	index := start + 1
	negate := false
	if index < len(pattern) && (pattern[index] == '!' || pattern[index] == '^') {
		negate = true
		index++
	}
	matched := false
	first := true
	for index < len(pattern) {
		if pattern[index] == ']' && !first {
			index++
			break
		}
		first = false
		if pattern[index] == '\\' && index+1 < len(pattern) {
			index++
		}
		low := pattern[index]
		index++
		if index+1 < len(pattern) && pattern[index] == '-' && pattern[index+1] != ']' {
			high := pattern[index+1]
			index += 2
			if character >= low && character <= high {
				matched = true
			}
			continue
		}
		if character == low {
			matched = true
		}
	}
	if negate {
		return !matched, index
	}
	return matched, index
}
