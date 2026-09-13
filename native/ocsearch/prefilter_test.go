package ocsearch

import (
	"fmt"
	"math/rand"
	"regexp"
	"testing"
)

func TestFoldUnsafeRunes(t *testing.T) {
	// 折叠等价类里含非 ASCII 的字符：按 ASCII 折叠搜索会漏匹配，不能进预筛。
	unsafe := []rune{'k', 'K', 's', 'S', 'é', '长', 'ſ', 'K'}
	for _, value := range unsafe {
		if foldSafeRune(value) {
			t.Errorf("%c 应判定为折叠不安全", value)
		}
	}
	safe := []rune{'a', 'i', 'y', 'o', 'z', '0', '_', '-', ':', 'A', 'Z'}
	for _, value := range safe {
		if !foldSafeRune(value) {
			t.Errorf("%c 应判定为折叠安全", value)
		}
	}
}

func TestExtractLiteralFilter(t *testing.T) {
	tests := []struct {
		name       string
		pattern    string
		ignoreCase bool
		needle     string
		folded     bool
	}{
		{"大小写敏感取最长段", `def\s+test\w*\(`, false, "test", false},
		{"忽略大小写取折叠安全段", `def\s+test\w*\(`, true, "def", true},
		{"跳过 k/s 等折叠不安全字符", "workspace", true, "pace", true},
		{"无 k/s 时整段可用", "handleAuth", true, "handleAuth", true},
		{"顶层选择放弃预筛", `messages|context`, true, "", false},
		{"锚定模式放弃预筛", `^def\s+`, true, "", false},
		{"可选量词后的字面量才算必需", `a?bcdef`, true, "bcdef", true},
		{"零最小重复丢弃", `ab{0,2}cdef`, true, "cdef", true},
		{"非零最小重复保留", `ab{3}cdef`, true, "cdef", true},
		{"字符集作为段边界", `[abc]defq`, true, "defq", true},
		{"行内开启忽略大小写", `(?i)config`, false, "config", true},
		{"命名分组放弃预筛", `(?P<name>abc)defg`, true, "", false},
		{"原样字面量区", `\Qfoo.bar\E`, false, "foo.bar", false},
		{"非 ASCII 只在敏感模式下可用", "café", false, "café", false},
		{"非 ASCII 折叠后取安全段", "café", true, "caf", true},
		{"转义字符属字面量", `foo\.bar`, false, "foo.bar", false},
		{"控制字符转义不作为字面量", `\nfoo`, false, "foo", false},
		{"过短字面量放弃预筛", `ab`, true, "", false},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			filter := extractLiteralFilter(test.pattern, test.ignoreCase)
			if test.needle == "" {
				if filter != nil {
					t.Fatalf("期望放弃预筛，实际 needle=%q", filter.needle)
				}
				return
			}
			if filter == nil {
				t.Fatalf("期望 needle=%q，实际放弃预筛", test.needle)
			}
			if string(filter.needle) != test.needle || filter.folded != test.folded {
				t.Errorf("得到 needle=%q folded=%v，期望 %q folded=%v",
					filter.needle, filter.folded, test.needle, test.folded)
			}
		})
	}
}

func TestIndexFoldASCII(t *testing.T) {
	tests := []struct {
		haystack string
		needle   string
		expected int
	}{
		{"Hello World", "hello", 0},
		{"xxWORLDyy", "world", 2},
		{"abc", "xyz", -1},
		{"", "a", -1},
		{"a", "a", 0},
		{"A", "a", 0},
		{"tail_needle", "NeEdLe", 5},
		{"ab", "abc", -1},
	}
	for _, test := range tests {
		got := indexFoldASCII([]byte(test.haystack), []byte(test.needle))
		if got != test.expected {
			t.Errorf("indexFoldASCII(%q, %q) = %d，期望 %d",
				test.haystack, test.needle, got, test.expected)
		}
	}
}

// TestIndexFoldASCIIMatchesNaive 用随机数据对照朴素的折叠搜索实现。
func TestIndexFoldASCIIMatchesNaive(t *testing.T) {
	random := rand.New(rand.NewSource(20260913))
	alphabet := []byte("abAB0_")
	for round := 0; round < 500; round++ {
		haystack := make([]byte, random.Intn(64))
		for index := range haystack {
			haystack[index] = alphabet[random.Intn(len(alphabet))]
		}
		needle := make([]byte, 1+random.Intn(6))
		for index := range needle {
			needle[index] = alphabet[random.Intn(len(alphabet))]
		}
		got := indexFoldASCII(haystack, needle)
		want := naiveFoldIndex(haystack, needle)
		if got != want {
			t.Fatalf("haystack=%q needle=%q 得到 %d，期望 %d", haystack, needle, got, want)
		}
	}
}

func naiveFoldIndex(haystack []byte, needle []byte) int {
	for start := 0; start+len(needle) <= len(haystack); start++ {
		matched := true
		for offset := range needle {
			if foldASCIILower(haystack[start+offset]) != foldASCIILower(needle[offset]) {
				matched = false
				break
			}
		}
		if matched {
			return start
		}
	}
	return -1
}

// TestPrefilterIsSound 是这次优化最关键的一条：预筛只允许提前排除"正则也匹配不
// 到"的行，绝不能让正则能匹配的行被挡掉。这里用正则的直接结果作为基准比对。
func TestPrefilterIsSound(t *testing.T) {
	patterns := []string{
		`config`,
		`def\s+test\w*\(`,
		`handleAuth\w*`,
		`[a-z]+Config`,
		`foo\d+Bar`,
		`\w*earch`,
		`workspace`,
		`(?i)MixedCase`,
		`café`,
		`a?bcdef`,
	}
	lines := []string{
		"", "Config", "CONFIG", "conFig file", "def  testX(", "handleAuthRequest",
		"aConfig", "foo12Bar", "Search", "seArch", "ſearch", "workſpace", "WORKSPACE",
		"workspace", "MixedCase", "mixedcase", "MIXEDCASE", "café", "CAFÉ", "caf\u00c9",
		"abcdef", "bcdef", "xbcdefy", "unrelated text", "ſ", "K", "K", "k",
	}
	for _, pattern := range patterns {
		for _, ignoreCase := range []bool{false, true} {
			expression := pattern
			if ignoreCase {
				expression = "(?i)" + expression
			}
			plain, err := regexp.Compile(expression)
			if err != nil {
				t.Fatalf("基准正则编译失败：%v", err)
			}
			compiled, err := buildMatcher(&options{
				mode:       modeMatches,
				pattern:    pattern,
				ignoreCase: ignoreCase,
			})
			if err != nil {
				t.Fatalf("buildMatcher(%q) 失败：%v", pattern, err)
			}
			for _, line := range lines {
				want := plain.MatchString(line)
				got := compiled.matchLine([]byte(line))
				if got != want {
					t.Errorf("模式 %q（忽略大小写=%v）行 %q：预筛结果 %v，正则结果 %v",
						pattern, ignoreCase, line, got, want)
				}
			}
		}
	}
}

// TestFixedStringFoldedMatcherMatchesRegex 校验字面量折叠快捷路径与正则等价。
func TestFixedStringFoldedMatcherMatchesRegex(t *testing.T) {
	literals := []string{"hello", "config", "handleAuth", "ab", "search", "café"}
	lines := []string{
		"Hello World", "CONFIG", "config", "handleauth", "HandleAuth", "ab",
		"AB", "ſearch", "café", "CAFÉ", "unrelated",
	}
	for _, literal := range literals {
		plain := regexp.MustCompile("(?i)" + regexp.QuoteMeta(literal))
		compiled, err := buildMatcher(&options{
			mode:         modeMatches,
			pattern:      literal,
			fixedStrings: true,
			ignoreCase:   true,
		})
		if err != nil {
			t.Fatalf("buildMatcher(%q) 失败：%v", literal, err)
		}
		for _, line := range lines {
			want := plain.MatchString(line)
			if got := compiled.matchLine([]byte(line)); got != want {
				t.Errorf("字面量 %q 行 %q：%v != %v", literal, line, got, want)
			}
		}
	}
}

func TestUsableNeedlePicksLongestFoldSafeRun(t *testing.T) {
	tests := []struct {
		text     string
		expected string
	}{
		{"workspace", "pace"},
		{"test", ""},
		{"handleAuth", "handleAuth"},
		{"asks", ""},
		{"café", "caf"},
	}
	for _, test := range tests {
		t.Run(fmt.Sprintf("%q", test.text), func(t *testing.T) {
			needle, ok := usableNeedle(literalRun{text: test.text, folded: true})
			if test.expected == "" {
				if ok {
					t.Fatalf("期望不可用，实际 needle=%q", needle)
				}
				return
			}
			if !ok || string(needle) != test.expected {
				t.Errorf("得到 %q(ok=%v)，期望 %q", needle, ok, test.expected)
			}
		})
	}
}
