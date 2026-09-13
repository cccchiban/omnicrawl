package ocsearch

import (
	"bytes"
	"unicode"
	"unicode/utf8"
)

// 预筛的作用：正则引擎（Go 的 RE2）在忽略大小写时用不上字面前缀优化，只能逐位置
// 尝试折叠匹配。这里先从模式里安全地抽出一个"每次匹配都必须出现"的字面量，用它
// 在进入正则引擎之前把整行排除掉。
//
// 安全第一：抽不出可靠字面量、或者字面量的折叠等价类里存在非 ASCII 字符
// （例如 ſ↔s、K↔k 这类 Unicode 简单折叠）时直接放弃预筛，宁可慢也不漏匹配。
const (
	// minExactLiteral 是大小写敏感预筛字面量的最小字节数：此时用 SIMD 的
	// bytes.Contains，两三个字节也划算。
	minExactLiteral = 3
	// minFoldedLiteral 是忽略大小写预筛字面量的最小字节数：折叠搜索只能逐字节
	// 比较，太短会把整行扫描变成纯开销。
	minFoldedLiteral = 3
)

// literalRun 是模式里一段连续的字面量，以及它所在作用域是否忽略大小写。
type literalRun struct {
	text   string
	folded bool
}

// indexFoldASCII 在 haystack 中查找 needle 的第一次出现，比较时只做 ASCII
// 大小写折叠。调用方必须保证 needle 里每个字符的折叠等价类都只含 ASCII。
func indexFoldASCII(haystack []byte, needle []byte) int {
	length := len(needle)
	if length == 0 {
		return 0
	}
	if len(haystack) < length {
		return -1
	}
	if length == 1 {
		lower := foldASCIILower(needle[0])
		upper := foldASCIIUpper(lower)
		for index, value := range haystack {
			if value == lower || value == upper {
				return index
			}
		}
		return -1
	}
	// Boyer-Moore-Horspool：移位表按折叠后的字节索引，扫描时也折叠比较。
	var shift [256]int
	for index := range shift {
		shift[index] = length
	}
	for index := 0; index < length-1; index++ {
		shift[foldASCIILower(needle[index])] = length - 1 - index
	}
	for position := 0; position+length <= len(haystack); {
		matched := true
		for offset := length - 1; offset >= 0; offset-- {
			if foldASCIILower(haystack[position+offset]) != foldASCIILower(needle[offset]) {
				matched = false
				break
			}
		}
		if matched {
			return position
		}
		position += shift[foldASCIILower(haystack[position+length-1])]
	}
	return -1
}

// foldASCIILower 把 ASCII 大写折叠成小写；其它字节原样返回。
func foldASCIILower(value byte) byte {
	if value >= 'A' && value <= 'Z' {
		return value + ('a' - 'A')
	}
	return value
}

// foldASCIIUpper 把 ASCII 小写折叠成大写；其它字节原样返回。
func foldASCIIUpper(value byte) byte {
	if value >= 'a' && value <= 'z' {
		return value - ('a' - 'A')
	}
	return value
}

// foldSafeRune 判断字符的折叠等价类是否只含 ASCII 字符。含非 ASCII 时按 ASCII
// 折叠搜索会漏匹配（例如 ſ 与 s、K 与 k 折叠等价），此时不能用于预筛。
func foldSafeRune(value rune) bool {
	if value >= utf8.RuneSelf {
		return false
	}
	for folded := unicode.SimpleFold(value); folded != value; folded = unicode.SimpleFold(folded) {
		if folded >= utf8.RuneSelf {
			return false
		}
	}
	return true
}

// usableNeedle 把一个字面量转成可用的预筛 needle：忽略大小写的字面量只能取其中
// 折叠安全的 ASCII 连续段；含控制字符的段一律丢弃（行文本里不会有它们）。
func usableNeedle(run literalRun) ([]byte, bool) {
	if !run.folded {
		if !foldSafeNeedleLiteral(run.text) {
			return nil, false
		}
		return []byte(run.text), true
	}
	var (
		best   string
		start  = -1
		length = 0
	)
	for index, value := range run.text {
		if foldSafeRune(value) && !isControlRune(value) {
			if start < 0 {
				start = index
			}
			continue
		}
		if start >= 0 {
			if index-start > length {
				best, length = run.text[start:index], index-start
			}
			start = -1
		}
	}
	if start >= 0 && len(run.text)-start > length {
		best, length = run.text[start:], len(run.text)-start
	}
	if length < minFoldedLiteral {
		return nil, false
	}
	return []byte(best), true
}

// foldSafeNeedleLiteral 判断大小写敏感的字面量能否直接作为 needle：只要求不含
// 控制字符（行文本里不会出现换行等），大小写敏感比较本身是精确的。
func foldSafeNeedleLiteral(text string) bool {
	if len(text) < minExactLiteral {
		return false
	}
	for _, value := range text {
		if isControlRune(value) {
			return false
		}
	}
	return true
}

// foldSafeWholeLiteral 判断整个字面量能否用 ASCII 折叠搜索精确替代忽略大小写的
// 字面量匹配：只有每个字符的折叠等价类都只含 ASCII 时才行。
func foldSafeWholeLiteral(text string) ([]byte, bool) {
	if len(text) < minFoldedLiteral {
		return nil, false
	}
	for _, value := range text {
		if !foldSafeRune(value) || isControlRune(value) {
			return nil, false
		}
	}
	return []byte(text), true
}

// isControlRune 判断是否为控制字符（含 \n、\t 等转义写法产生的字符）。
func isControlRune(value rune) bool {
	return value < 0x20 || value == 0x7f
}

// extractLiteralFilter 从正则模式里安全地抽出一个必需字面量用于预筛；
// 无法保证安全时返回 nil，此时完全交由正则引擎决定结果。
func extractLiteralFilter(pattern string, ignoreCase bool) *literalFilter {
	if anchoredAtStart(pattern) {
		// 以 ^/\A 开头的模式在位置 0 就能快速失败，预筛反而要白扫整行。
		return nil
	}
	runs, ok := scanLiteralRuns(pattern, ignoreCase)
	if !ok {
		return nil
	}
	var best *literalFilter
	for _, run := range runs {
		needle, usable := usableNeedle(run)
		if !usable {
			continue
		}
		candidate := &literalFilter{needle: needle, folded: run.folded}
		if best == nil || candidate.weight() > best.weight() {
			best = candidate
		}
	}
	return best
}

// literalFilter 是预筛字面量。
type literalFilter struct {
	needle []byte
	folded bool
}

// weight 用于在多个候选字面量间取舍：精确匹配走 SIMD，比折叠搜索更快，因此
// 同等长度优先精确匹配。
func (filter *literalFilter) weight() int {
	if filter.folded {
		return len(filter.needle)
	}
	return len(filter.needle) * 2
}

// scanLiteralRuns 扫描模式文本，收集"每次匹配都必须出现"的字面量段。
//
// 只在最外层（不在任何分组、字符集内）收集，并且跳过最小重复次数为 0 的量词；
// 遇到无法安全解析的构造（顶层选择、命名分组、环视等）直接放弃整个预筛。
func scanLiteralRuns(pattern string, ignoreCase bool) ([]literalRun, bool) {
	runes := []rune(pattern)
	folded := ignoreCase
	var (
		runs    []literalRun
		current []rune
		depth   int
	)
	// flush 收尾当前字面量段：只有最外层的段才可能是"必需"的。
	flush := func() {
		if depth == 0 && len(current) > 0 {
			runs = append(runs, literalRun{text: string(current), folded: folded})
		}
		current = current[:0]
	}
	drop := func() {
		current = current[:0]
	}

	for index := 0; index < len(runes); {
		value := runes[index]
		switch value {
		case '\\':
			if index+1 >= len(runes) {
				return nil, false
			}
			next := runes[index+1]
			switch {
			case next == 'Q':
				// \Q...\E 是原样字面量区。
				end := index + 2
				for end < len(runes) && !(runes[end] == '\\' && end+1 < len(runes) && runes[end+1] == 'E') {
					end++
				}
				current = append(current, runes[index+2:end]...)
				index = end
				if index < len(runes) {
					index += 2
				}
				continue
			case !isAlphaNumericRune(next):
				// \. \\ \+ 这类转义就是该字符本身。
				current = append(current, next)
				index += 2
				continue
			default:
				// \d \w \s \b \p{...} \x41 ... 都不是字面量，作为段边界处理。
				flush()
				index = skipEscapePayload(runes, index+1)
				continue
			}
		case '[':
			flush()
			end, ok := skipCharacterClass(runes, index)
			if !ok {
				return nil, false
			}
			index = end
			continue
		case '(':
			flush()
			index++
			if index < len(runes) && runes[index] == '?' {
				consumed, updated, ok := parseGroupPrefix(runes, index)
				if !ok {
					return nil, false
				}
				if updated != nil {
					folded = *updated
				} else {
					depth++
				}
				index = consumed
				continue
			}
			depth++
			continue
		case ')':
			if depth == 0 {
				return nil, false
			}
			flush()
			drop()
			depth--
			index++
			continue
		case '|':
			if depth == 0 {
				// 顶层选择：整体不再保证包含某个固定字面量。
				return nil, false
			}
			drop()
			index++
			continue
		case '*', '?':
			// 最小重复次数为 0：紧邻的字面量不是必需的。
			drop()
			index++
			continue
		case '+':
			flush()
			index++
			continue
		case '{':
			minRepeats, end, isQuantifier := parseBraceQuantifier(runes, index)
			switch {
			case !isQuantifier:
				flush()
			case minRepeats == 0:
				drop()
			default:
				flush()
			}
			index = end
			continue
		case '^', '$', '.', '}', ']':
			flush()
			index++
			continue
		default:
			current = append(current, value)
			index++
		}
	}
	if depth != 0 {
		return nil, false
	}
	flush()
	return runs, true
}

// parseGroupPrefix 解析 "(?" 之后的构造，返回下一个待处理下标。flagsOnly 为真时
// （(?i) / (?-i) 形式）返回新的忽略大小写状态，否则按普通分组递增深度。
func parseGroupPrefix(runes []rune, index int) (int, *bool, bool) {
	// index 指向 '?'。
	cursor := index + 1
	if cursor < len(runes) && runes[cursor] == ':' {
		return cursor + 1, nil, true
	}
	flagStart := cursor
	for cursor < len(runes) && (runes[cursor] == '-' || isRegexFlagRune(runes[cursor])) {
		cursor++
	}
	if cursor > flagStart && cursor < len(runes) && runes[cursor] == ')' {
		flags := string(runes[flagStart:cursor])
		updated := applyIgnoreCaseFlags(flags)
		return cursor + 1, &updated, true
	}
	// (?P<name> / (?= / (?<= / (?> 等：不支持解析，放弃预筛。
	return 0, nil, false
}

// applyIgnoreCaseFlags 根据 (?flags) 形式的开关更新忽略大小写状态：
// "-i" 关闭，"i" 开启，其余标志（m/s/U/x）与字面量匹配无关。
func applyIgnoreCaseFlags(flags string) bool {
	if len(flags) >= 2 && flags[0] == '-' {
		return false
	}
	for _, value := range flags {
		if value == '-' {
			break
		}
		if value == 'i' {
			return true
		}
	}
	return false
}

// skipEscapePayload 跳过 \p{...} / \x{...} / \xNN / \uNNNN 的载荷，返回下一个
// 待处理下标。
func skipEscapePayload(runes []rune, index int) int {
	// index 指向转义字符本身。
	cursor := index + 1
	if cursor >= len(runes) {
		return cursor
	}
	switch runes[index] {
	case 'p', 'P':
		if cursor < len(runes) && runes[cursor] == '{' {
			cursor++
			for cursor < len(runes) && runes[cursor] != '}' {
				cursor++
			}
			if cursor < len(runes) {
				cursor++
			}
		}
		return cursor
	case 'x', 'u', 'U':
		if cursor < len(runes) && runes[cursor] == '{' {
			for cursor < len(runes) && runes[cursor] != '}' {
				cursor++
			}
			if cursor < len(runes) {
				cursor++
			}
			return cursor
		}
		for cursor < len(runes) && isHexRune(runes[cursor]) {
			cursor++
		}
		return cursor
	default:
		return cursor
	}
}

// skipCharacterClass 跳过 "[...]" 字符集，返回其后的下标。
func skipCharacterClass(runes []rune, index int) (int, bool) {
	// index 指向 '['。
	cursor := index + 1
	if cursor < len(runes) && runes[cursor] == '^' {
		cursor++
	}
	if cursor < len(runes) && runes[cursor] == ']' {
		cursor++
	}
	for cursor < len(runes) {
		switch runes[cursor] {
		case '\\':
			cursor += 2
			continue
		case ']':
			return cursor + 1, true
		default:
			cursor++
		}
	}
	return 0, false
}

// parseBraceQuantifier 解析 "{n}" / "{n,}" / "{n,m}"，返回最小重复次数与结束下标；
// 不是合法量词时 isQuantifier 为 false（此时 '{' 在 RE2 里就是普通字符）。
func parseBraceQuantifier(runes []rune, index int) (int, int, bool) {
	cursor := index + 1
	start := cursor
	value := 0
	for cursor < len(runes) && runes[cursor] >= '0' && runes[cursor] <= '9' {
		value = value*10 + int(runes[cursor]-'0')
		cursor++
	}
	if cursor == start {
		return 0, cursor, false
	}
	if cursor < len(runes) && runes[cursor] == '}' {
		return value, cursor + 1, true
	}
	if cursor < len(runes) && runes[cursor] == ',' {
		cursor++
		for cursor < len(runes) && runes[cursor] >= '0' && runes[cursor] <= '9' {
			cursor++
		}
		if cursor < len(runes) && runes[cursor] == '}' {
			return value, cursor + 1, true
		}
	}
	return 0, cursor, false
}

// anchoredAtStart 判断模式是否以 ^ 或 \A 开头（允许前面有 (?flags) 开关组）。
func anchoredAtStart(pattern string) bool {
	runes := []rune(pattern)
	index := 0
	for index < len(runes) && runes[index] == '(' && index+1 < len(runes) && runes[index+1] == '?' {
		consumed, updated, ok := parseGroupPrefix(runes, index+1)
		if !ok || updated == nil {
			return false
		}
		index = consumed
	}
	if index >= len(runes) {
		return false
	}
	if runes[index] == '^' {
		return true
	}
	return runes[index] == '\\' && index+1 < len(runes) && runes[index+1] == 'A'
}

// isRegexFlagRune 判断是否为 (?imsUx) 允许的标志字符。
func isRegexFlagRune(value rune) bool {
	switch value {
	case 'i', 'm', 's', 'U', 'x':
		return true
	default:
		return false
	}
}

// isAlphaNumericRune 判断是否为 ASCII 字母或数字（用于区分 \d 与 \.）。
func isAlphaNumericRune(value rune) bool {
	return (value >= 'a' && value <= 'z') ||
		(value >= 'A' && value <= 'Z') ||
		(value >= '0' && value <= '9')
}

// isHexRune 判断是否为十六进制数字。
func isHexRune(value rune) bool {
	switch {
	case value >= '0' && value <= '9':
		return true
	case value >= 'a' && value <= 'f':
		return true
	case value >= 'A' && value <= 'F':
		return true
	default:
		return false
	}
}

// containsNeedle 是预筛的实际判断：精确 needle 走 SIMD 子串查找，
// 折叠 needle 走 ASCII 折叠搜索。
func containsNeedle(line []byte, filter *literalFilter) bool {
	if filter.folded {
		return indexFoldASCII(line, filter.needle) >= 0
	}
	return bytes.Contains(line, filter.needle)
}
