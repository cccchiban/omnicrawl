#!/usr/bin/env python3
"""模型上下文窗口校对（对照 models.dev 公开模型表）。

把本机 `~/.OmniCrawl/models.toml` 与 `config.toml` 里声明的窗口，和 models.dev
的公开发布值核一遍，输出差异报告。只读本机配置，不写任何配置文件。

同一个 model_id 在不同渠道下的窗口并不相同（同一模型可能同时有 1000000 与
1048576 两种登记），因此判据是「配置值是否落在该模型登记的候选集内」，
而不是「与唯一权威值相等」。命中候选集只说明本机值与某个公开渠道一致。

用法：
    python rust/scripts/model_windows.py                # 联网拉表并报告
    python rust/scripts/model_windows.py --save snap.json   # 同时留一份快照
    python rust/scripts/model_windows.py --table snap.json  # 用本地快照离线校对
    python rust/scripts/model_windows.py --config-dir D:/cfg

退出码：0 = 未发现偏离；1 = 有窗口不在候选集内或模型查不到；2 = 运行失败。
"""
import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.request
from collections import Counter

MODELS_DEV_URL = "https://models.dev/api.json"
FETCH_TIMEOUT_SECONDS = 60
USER_AGENT = "omnicrawl-model-windows/1"


def load_toml_module():
    try:
        import tomllib

        return tomllib
    except ModuleNotFoundError:
        pass
    try:
        import tomli

        return tomli
    except ModuleNotFoundError:
        raise SystemExit(
            "需要 tomllib（Python 3.11+）或 tomli 才能读取配置：pip install tomli"
        )


def read_toml(path):
    if not os.path.isfile(path):
        return None
    with open(path, "rb") as handle:
        raw = handle.read()
    return load_toml_module().loads(raw.decode("utf-8"))


def normalize(model_id):
    """把模型标识折成可比较的形态：小写、分隔符统一成连字符。

    models.dev 用 `deepseek-v4-1-flash` 登记 `deepseek-v4.1-flash`，点号必须先折叠，
    否则同名模型会被判成查不到。
    """
    text = (model_id or "").strip().lower()
    text = re.sub(r"[^a-z0-9]+", "-", text)
    return text.strip("-")


def fetch_table(path=None):
    if path:
        with open(path, "r", encoding="utf-8") as handle:
            return json.load(handle)
    request = urllib.request.Request(MODELS_DEV_URL, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(request, timeout=FETCH_TIMEOUT_SECONDS) as response:
        return json.load(response)


def index_table(table):
    """model_id 归一化名 → 候选窗口集合与提供该值的渠道。

    候选值按 provider 计数，报告里才能说清「哪个渠道给的是哪个值」。
    """
    index = {}
    for provider_key, provider in table.items():
        for model in (provider.get("models") or {}).values():
            limit = model.get("limit") or {}
            context = limit.get("context")
            output = limit.get("output")
            if not context:
                continue
            name = normalize(model.get("id"))
            if not name:
                continue
            entry = index.setdefault(
                name, {"context": Counter(), "output": Counter(), "providers": {}}
            )
            entry["context"][context] += 1
            if output:
                entry["output"][output] += 1
            entry["providers"].setdefault(context, []).append(provider_key)
    return index


def collect_config_models(config_dir):
    """收集待校对的条目：自定义模型条目 + 渠道默认值。

    窗口为 0 表示继承，按多模型解析顺序折算成实际生效值：
    条目值 → 渠道默认 → 全局默认。
    """
    config_path = os.path.join(config_dir, "config.toml")
    models_path = os.path.join(config_dir, "models.toml")
    config = read_toml(config_path) or {}
    store = read_toml(models_path) or {}

    llm = config.get("llm") or {}
    defaults = llm.get("defaults") or {}
    global_default = defaults.get("context_window_tokens") or 0
    profiles = llm.get("profiles") or {}

    entries = []
    for key, record in (store.get("models") or {}).items():
        if not isinstance(record, dict):
            continue
        profile = profiles.get(record.get("profile")) or {}
        profile_default = profile.get("default_context_window_tokens") or 0
        window, source = 0, "未声明"
        for label, value in (
            ("条目录入", record.get("context_window_tokens") or 0),
            ("渠道默认", profile_default),
            ("全局默认", global_default),
        ):
            if value:
                window, source = value, label
                break
        entries.append(
            {
                "kind": "模型",
                "key": key,
                "model_id": record.get("model_id") or key,
                "window": window,
                "window_source": source,
                "output": record.get("max_output_tokens") or 0,
                "profile": record.get("profile") or "",
                "base_url": profile.get("base_url") or "",
            }
        )

    # 只配了渠道、没有任何自定义条目时，渠道默认就是实际生效的窗口。
    if not entries:
        for name, profile in profiles.items():
            if not isinstance(profile, dict):
                continue
            window = profile.get("default_context_window_tokens") or global_default
            if window:
                entries.append(
                    {
                        "kind": "渠道默认",
                        "key": name,
                        "model_id": name,
                        "window": window,
                        "window_source": "渠道默认",
                        "output": 0,
                        "profile": name,
                        "base_url": profile.get("base_url") or "",
                    }
                )
    return entries


def judge(entry, index):
    """判定一条配置：一致 / 偏离 / 查不到。候选集按窗口值降序排列。"""
    hit = index.get(normalize(entry["model_id"]))
    if hit is None:
        return {"verdict": "查不到", "candidates": [], "providers": {}}
    candidates = sorted(hit["context"].keys(), reverse=True)
    verdict = "一致" if entry["window"] in hit["context"] else "偏离"
    return {
        "verdict": verdict,
        "candidates": candidates,
        "providers": hit["providers"],
        "output_candidates": sorted(hit["output"].keys(), reverse=True),
    }


def human_tokens(value):
    if not value:
        return "-"
    if value % 1024 == 0:
        return "{:,}（{}K）".format(value, value // 1024)
    return "{:,}".format(value)


def render(entries, results):
    lines = []
    if not entries:
        lines.append("没有找到可校对的模型条目（models.toml 与渠道默认都为空）。")
        return lines

    lines.append(
        "{:<16}{:<26}{:>18}{:<8}{}".format("配置键", "model_id", "生效窗口", "来源", "结论")
    )
    lines.append("-" * 92)
    for entry, result in zip(entries, results):
        lines.append(
            "{:<16}{:<26}{:>18}{:<8}{}".format(
                entry["key"][:15],
                entry["model_id"][:25],
                human_tokens(entry["window"]),
                entry["window_source"],
                result["verdict"],
            )
        )

    deviations = [
        (entry, result)
        for entry, result in zip(entries, results)
        if result["verdict"] != "一致"
    ]
    if not deviations:
        lines.append("")
        lines.append("全部窗口都在 models.dev 的登记候选集内。")
        return lines

    lines.append("")
    lines.append("需要人工核对：")
    for entry, result in deviations:
        lines.append("")
        lines.append("  {}（{}）".format(entry["key"], entry["model_id"]))
        if entry["base_url"]:
            lines.append("    渠道           {}".format(entry["base_url"]))
        lines.append("    本机生效窗口   {}".format(human_tokens(entry["window"])))
        if result["verdict"] == "查不到":
            lines.append("    登记           该 model_id 在 models.dev 查不到，无法校对")
            lines.append("    提示           自建渠道或私有模型名属正常；公共模型请核对拼写")
            continue
        lines.append(
            "    表中候选       {}".format(
                "、".join(human_tokens(value) for value in result["candidates"][:6])
            )
        )
        if result.get("output_candidates"):
            lines.append(
                "    表中输出上限   {}".format(
                    "、".join(human_tokens(value) for value in result["output_candidates"][:4])
                )
            )
        detail = []
        for value in result["candidates"][:4]:
            owners = result["providers"].get(value) or []
            if owners:
                detail.append("{} -> {}".format(value, "、".join(owners[:3])))
        if detail:
            lines.append("    候选来源       {}".format("；".join(detail)))
        lines.append(
            "    说明           同一 model_id 在不同渠道登记的窗口不同，配置值只需与其中一档相符"
        )
        lines.append(
            "    影响           窗口决定压缩触发点（窗口 x trigger_context_percent）；"
            "填大则阈值偏高，可能先撞上游硬上限"
        )
    return lines


def main(argv=None):
    # Windows 控制台默认代码页会让中文输出崩掉，与 size_guard.py 同一处理。
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            try:
                stream.reconfigure(encoding="utf-8")
            except Exception:
                pass

    parser = argparse.ArgumentParser(description="对照 models.dev 校对模型上下文窗口")
    parser.add_argument(
        "--config-dir", default=os.path.join(os.path.expanduser("~"), ".OmniCrawl")
    )
    parser.add_argument("--table", help="用本地 models.dev 快照代替联网拉取")
    parser.add_argument("--save", help="把拉到的表另存为快照，供 --table 离线复用")
    args = parser.parse_args(argv)

    try:
        table = fetch_table(args.table)
    except (urllib.error.URLError, OSError, ValueError) as error:
        print("读取 models.dev 失败：{}".format(error), file=sys.stderr)
        return 2

    if args.save:
        with open(args.save, "w", encoding="utf-8") as handle:
            json.dump(table, handle, ensure_ascii=False)
        print("已保存模型表快照：{}".format(args.save))

    index = index_table(table)
    entries = collect_config_models(args.config_dir)
    results = [judge(entry, index) for entry in entries]

    print(
        "模型表：{} 个渠道 / {} 个模型登记".format(
            len(table), sum(len(provider.get("models") or {}) for provider in table.values())
        )
    )
    print("配置：{}".format(args.config_dir))
    print("")
    for line in render(entries, results):
        print(line)

    return 0 if all(result["verdict"] == "一致" for result in results) else 1


if __name__ == "__main__":
    sys.exit(main())
