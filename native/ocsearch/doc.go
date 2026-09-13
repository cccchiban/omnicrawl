// Package ocsearch 是随 wheel 分发的原生文本搜索实现：用 Go 重写了此前由
// ripgrep 二进制承担的 grep/find 能力，去掉随包分发的 5 个平台二进制
// （约 26MB），同时保留 ripgrep 的关键行为：.gitignore/.ignore 剪枝、
// 隐藏文件可见、basename/路径 glob、每文件计数与仅列文件。
package ocsearch
