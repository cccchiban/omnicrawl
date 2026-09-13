// Command cmod 把 ocsearch 暴露成 C ABI，再由同包的 module.c 挂成 CPython
// 原生扩展（abi3 稳定 ABI，一份产物跨 Python 3.9+ 使用）。
//
// 构建入口是 native/build.py：它负责带上 Python 头文件/导入库的 CGO 参数，
// 直接 go build 本包会缺少 Python 头文件而失败。
package main

/*
#include <stdlib.h>
*/
import "C"

import (
	"encoding/json"
	"errors"
	"fmt"
	"time"

	"github.com/cccchiban/omnicrawl/native/ocsearch"
)

// request 是 Python 侧传入的一次搜索调用。
type request struct {
	Args           []string `json:"args"`
	Cwd            string   `json:"cwd"`
	TimeoutSeconds float64  `json:"timeout_seconds"`
}

// response 是返回给 Python 侧的结构化结果：stdout 与退出码语义与 ripgrep
// 一致，error 非空时调用方直接把它当成工具错误抛出。
type response struct {
	Stdout     string `json:"stdout"`
	Returncode int    `json:"returncode"`
	Error      string `json:"error,omitempty"`
	TimedOut   bool   `json:"timed_out,omitempty"`
}

//export oc_search
func oc_search(rawRequest *C.char) *C.char {
	var parsed request
	if err := json.Unmarshal([]byte(C.GoString(rawRequest)), &parsed); err != nil {
		return encodeResponse(response{Returncode: 2, Error: fmt.Sprintf("搜索请求解析失败：%v", err)})
	}
	var timeout time.Duration
	if parsed.TimeoutSeconds > 0 {
		timeout = time.Duration(parsed.TimeoutSeconds * float64(time.Second))
	}
	stdout, code, err := ocsearch.Run(parsed.Args, parsed.Cwd, timeout)
	result := response{Stdout: stdout, Returncode: code}
	if err != nil {
		result.Error = err.Error()
		result.TimedOut = errors.Is(err, ocsearch.ErrTimeout)
	}
	return encodeResponse(result)
}

// opVersion 返回扩展版本，供 Python 侧做后端可用性探测。
//
//export oc_version
func oc_version() *C.char {
	return C.CString(version)
}

// version 与 pyproject.toml 的版本保持独立：它只标记原生搜索核心的协议版本，
// Python 侧据此判断扩展与包装层是否匹配（不兼容时回退到 PATH 上的 rg）。
const version = "1"

// encodeResponse 把响应编码成 C 字符串；调用方（module.c）负责 free。
func encodeResponse(value response) *C.char {
	encoded, err := json.Marshal(value)
	if err != nil {
		return C.CString(`{"stdout":"","returncode":2,"error":"搜索响应编码失败"}`)
	}
	return C.CString(string(encoded))
}

func main() {}
