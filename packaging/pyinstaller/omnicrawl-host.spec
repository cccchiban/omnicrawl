# OmniCrawl 宿主载荷（one-dir）：Python 运行时 + 依赖 + 代码 + 资源打成同一目录。
#
# 用 one-dir 而不是 one-file：Textual 需要真实终端、uvicorn/fastapi 动态导入多、
# omnicrawl._ocsearch 是就地编译的扩展、模板与模型按包内路径读取，自解压式启动会
# 把它们逐一炸掉。载荷由 packages/cli/scripts/build-host.mjs 调用产出。
#
# 收集策略是「静态分析 + 精确清单」，不用 collect_all：整包收集会把 onnxruntime 的
# 量化子模块之类顺带拖进来，连带 numba/llvmlite 与按 numpy 1.x 编译的模块，载荷直接
# 涨到 800 MB 以上且进包即崩。
from pathlib import Path

from PyInstaller.utils.hooks import collect_data_files, collect_dynamic_libs, collect_submodules

SPEC_DIR = Path(SPECPATH).resolve()

# 包内非 .py 资源（templates/、config/templates/、ui/fullscreen/status/、*.pt、
# gitleaks.toml、extensions/node_runner.mjs）由 collect_data_files 统一收集。
datas = list(collect_data_files("omnicrawl", include_py_files=False))
binaries = []

# 静态分析看不见的导入面，缺一个就在运行期炸在 import 上。
hiddenimports = [
    "omnicrawl._ocsearch",
    "uvicorn.logging",
    "uvicorn.loops.auto",
    "uvicorn.protocols.http.auto",
    "uvicorn.protocols.websockets.auto",
    "uvicorn.lifespan.on",
    "uvicorn.lifespan.off",
    # 可选能力：TTS、消息脱敏 NER、各家模型 SDK、配置文件解析。
    "onnxruntime",
    "sentencepiece",
    "anthropic",
    "openai",
    "yaml",
    "bs4",
    "markdown",
]

# 本包内部的兼容别名与按需加载都走 importlib（见 omnicrawl/__init__.py 的
# _COMPAT_MODULES 与 _LAZY_COMPAT_MODULES）：静态分析看不见这些目标，自家包整包收编。
hiddenimports += collect_submodules("omnicrawl")

# Textual 的 CSS 与控件按名字装载；google.genai 是命名空间包，子模块要显式收。
datas += collect_data_files("textual")
hiddenimports += collect_submodules("textual")
hiddenimports += collect_submodules("google.genai")

# curl_cffi 带 libcurl 动态库，PyInstaller 没有对应的内置 hook。
datas += collect_data_files("curl_cffi")
binaries += collect_dynamic_libs("curl_cffi")

# 构建期硬门槛：资源面缺了就在打包阶段失败，别等用户机器上才发现。
# PyInstaller 6 的数据项是 (源文件, 目标目录)。
sources = {Path(entry[0]).as_posix() for entry in datas}
for required in (
    "omnicrawl/templates/summary_prompt.md",
    "omnicrawl/extensions/node_runner.mjs",
    "omnicrawl/llm/desensitization/gitleaks.toml",
    "omnicrawl/llm/desensitization/models/bilstm_crf_best.pt",
    "omnicrawl/config_chat/assets/router.pt",
):
    if not any(path.endswith(required) for path in sources):
        raise SystemExit(f"[spec] 资源缺失：{required}（检查 collect_data_files 的收集结果）")

analysis = Analysis(
    [str(SPEC_DIR / "host_entry.py")],
    pathex=[str(SPEC_DIR.parents[1])],
    binaries=binaries,
    datas=datas,
    hiddenimports=hiddenimports,
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[
        # 与 OmniCrawl 无关的重物：绘图、数据栈、交互式环境，以及 onnxruntime 量化
        # 子模块牵连进来的 numba/llvmlite（按 numpy 1.x 编译，进包必炸）。
        "tkinter",
        "matplotlib",
        "pandas",
        "numexpr",
        "bottleneck",
        "scipy",
        "numba",
        "llvmlite",
        "torch",
        "IPython",
        "jupyter",
        "notebook",
        "pytest",
        "sphinx",
    ],
    noarchive=False,
)

# 构建机可能装的是 onnxruntime-gpu：hook 会把 CUDA/TensorRT provider 一起收进来
# （单个 onnxruntime_providers_cuda.dll 就有 ~580 MB）。载荷只带 CPU 推理，
# CUDA 版由用户在设置页按需安装。
analysis.binaries = [
    entry
    for entry in analysis.binaries
    if "onnxruntime_providers_" not in entry[0] or "providers_shared" in entry[0]
]

pyz = PYZ(analysis.pure, analysis.zipped_data)
exe = EXE(
    pyz,
    analysis.scripts,
    [],
    exclude_binaries=True,
    name="omnicrawl-host",
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=False,
    console=True,
    disable_windowed_traceback=False,
)

coll = COLLECT(
    exe,
    analysis.binaries,
    analysis.datas,
    strip=False,
    upx=False,
    name="omnicrawl-host",
)
