/* CPython 侧入口：只负责 JSON 字符串的搬运与 GIL 释放，搜索逻辑全部在 Go 侧。
 *
 * 使用稳定 ABI（Py_LIMITED_API），因此同一份产物可以跨 Python 3.9+ 使用，
 * 不需要按解释器版本各出一份 wheel。
 *
 * 未定义 OCSEARCH_WITH_PYTHON 时只编译占位实现，让 `go vet ./...` /
 * `go build ./...` 在没有 Python 头文件的机器上也能跑通；真正的扩展由
 * native/build.py 带上 -DOCSEARCH_WITH_PYTHON 构建。
 */
#ifdef OCSEARCH_WITH_PYTHON

#define Py_LIMITED_API 0x03090000
#include <Python.h>
#include <stdlib.h>

extern char *oc_search(char *request);
extern char *oc_version(void);

static PyObject *py_run(PyObject *self, PyObject *args) {
    const char *request = NULL;
    if (!PyArg_ParseTuple(args, "s", &request)) {
        return NULL;
    }
    char *response = NULL;
    /* 搜索可能持续数秒，释放 GIL 以免阻塞 TUI 等其它线程。 */
    Py_BEGIN_ALLOW_THREADS
    response = oc_search((char *)request);
    Py_END_ALLOW_THREADS
    if (response == NULL) {
        PyErr_SetString(PyExc_RuntimeError, "native search returned no response");
        return NULL;
    }
    PyObject *result = PyUnicode_FromString(response);
    free(response);
    return result;
}

static PyObject *py_version(PyObject *self, PyObject *args) {
    char *value = oc_version();
    if (value == NULL) {
        Py_RETURN_NONE;
    }
    PyObject *result = PyUnicode_FromString(value);
    free(value);
    return result;
}

static PyMethodDef ocsearch_methods[] = {
    {"run", py_run, METH_VARARGS,
     "Run one ripgrep-compatible search request (JSON in, JSON out)."},
    {"version", py_version, METH_NOARGS, "Return the native search core version."},
    {NULL, NULL, 0, NULL},
};

static struct PyModuleDef ocsearch_module = {
    PyModuleDef_HEAD_INIT,
    "_ocsearch",
    "Native (Go) search backend for the omnicrawl workspace tools.",
    -1,
    ocsearch_methods,
};

PyMODINIT_FUNC PyInit__ocsearch(void) {
    return PyModule_Create(&ocsearch_module);
}

#else

int ocsearch_module_placeholder(void) {
    return 0;
}

#endif /* OCSEARCH_WITH_PYTHON */
