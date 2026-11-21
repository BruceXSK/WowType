#!/usr/bin/env python3
"""设置契约：拿 spec/config.json、spec/lexicon-pack.json 对照 Linux 的设置界面和 fcitx5 插件。

  python3 scripts/test-config-contract.py                                   # 规格自检 + 设置界面
  python3 scripts/test-config-contract.py --fcitx5 <wowtype-config-dump>    # 再对照 fcitx5 插件
  python3 scripts/test-config-contract.py --windows <wowtype-windows-config-dump>  # 规格自检 + Windows 外壳

wowtype-config-dump 是 scripts/addon-test.sh 编出来的（fcitx5-addon/test/config_dump.cpp），导出插件里
WowtypeConfig 的键、类型、默认值、范围、可选值。macOS 那端在 macos/test.sh 里对照同一份规格。
wowtype-windows-config-dump 是 windows/test.sh（或 CI 里 CMake）编出来的（windows/tests/config_dump.cpp），
导出 Windows 外壳的设置项表、按文件读出来的值。--windows 只对照规格和 Windows（Windows 上没有 GTK 的设置界面可导）。

设置界面（settings/wowtype-config）没有 .py 后缀，用 importlib 按路径导入。不用装 GTK：导入前塞一个假的 gi，
它在模块顶层只用到 gi.require_version、几个当基类的控件和 GObject.Property。XDG 目录都指到临时目录，不碰自己的配置。
"""

import hashlib
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile
import types

ROOT = Path(__file__).resolve().parents[1]
SPEC_DIR = ROOT / "spec"
SETTINGS = ROOT / "settings" / "wowtype-config"
ENDS = ("fcitx5", "settings", "macos", "windows")
TYPES = ("bool", "int", "enum", "string", "key")
# fcitx5 配置说明里的类型名
FCITX_TYPES = {"Boolean": "bool", "Integer": "int", "Enum": "enum", "String": "string", "Key": "key"}

failures = []
checked = 0


def check(ok, what, detail=""):
    global checked
    checked += 1
    if ok:
        return True
    failures.append(f"{what}" + (f"\n    {detail}" if detail else ""))
    return False


def same(actual, expected, what):
    return check(actual == expected, what, f"规格：{expected!r}\n    实际：{actual!r}")


def same_set(actual, expected, what):
    actual, expected = set(actual), set(expected)
    return check(actual == expected, what,
                 f"规格里有、这边没有：{sorted(expected - actual)}\n    这边有、规格里没有：{sorted(actual - expected)}")


# ---------------------------------------------------------------- 规格


def load_spec(name):
    with open(SPEC_DIR / name, encoding="utf-8") as f:
        return json.load(f)


def file_text(value):
    """规格里的值写进 wowtype.conf 的样子。"""
    if isinstance(value, bool):
        return "True" if value else "False"
    return str(value)


def default_for(setting, end):
    return file_text(setting.get("platformDefault", {}).get(end, setting["default"]))


def enum_values(setting):
    return [v if isinstance(v, str) else v["value"] for v in setting["values"]]


def settings_for(spec, end):
    return [s for s in spec["settings"] if end in s["platforms"]]


def out_of_range_cases(setting, policy, end):
    """数字项：(写进文件的值, 读出来应该是什么)。"""
    low, high = setting["min"], setting["max"]
    default = default_for(setting, end)
    cases = [(str(low), str(low)), (str(high), str(high))]
    for raw, edge in ((low - 1, low), (high + 1, high)):
        expect = {"default": default, "keep": str(raw), "clamp": str(edge)}[policy]
        cases.append((str(raw), expect))
    return cases


def index_prompt(prompt, source, target_name, formal):
    """按规格拼 Index-Translate 的提示词；target_name 是 None 时原文是外文（译成中文）。"""
    head = prompt["headChinese"].replace("{target}", target_name) if target_name else prompt["headOther"]
    tone = prompt["toneFormal" if formal else "toneCasual"]
    return prompt["template"].replace("{head}", head).replace("{tone}", tone).replace("{text}", source.strip())


def check_spec(spec, pack):
    """规格本身说得通：键不重复、默认值合类型、在范围和可选值里，例子用到的键都有。"""
    keys = [s["key"] for s in spec["settings"]]
    check(len(keys) == len(set(keys)), "规格：键重复", sorted(k for k in keys if keys.count(k) > 1))
    by_key = {s["key"]: s for s in spec["settings"]}
    for s in spec["settings"]:
        key, kind, default = s["key"], s["type"], s["default"]
        if not check(kind in TYPES, f"规格：{key} 的类型 {kind!r} 不认识"):
            continue
        check(s["platforms"] and set(s["platforms"]) <= set(ENDS), f"规格：{key} 的 platforms 不对", s["platforms"])
        for end, value in s.get("platformDefault", {}).items():
            check(end in s["platforms"], f"规格：{key} 的 platformDefault 写了没有这一项的 {end}")
            check(type(value) is type(default), f"规格：{key} 在 {end} 的默认值类型不对", value)
            check(bool(s.get("why")), f"规格：{key} 两端默认值不一样，要写 why")
        expected_type = {"bool": bool, "int": int, "enum": str, "string": str, "key": str}[kind]
        check(type(default) is expected_type, f"规格：{key} 的默认值不是 {kind}", default)
        if kind == "int":
            check(s["min"] <= default <= s["max"], f"规格：{key} 的默认值不在 {s['min']}–{s['max']} 里", default)
        if kind == "enum":
            values = enum_values(s)
            check(len(values) == len(set(values)), f"规格：{key} 的可选值重复", values)
            check(default in values, f"规格：{key} 的默认值不在可选值里", default)
        if "choices" in s:
            # 数字项的下拉框（VoiceSilenceTimeout）：选项在范围里、不重复、有默认值，三端用同一套名字和说明
            values = [c["value"] for c in s["choices"]]
            check(kind == "int", f"规格：{key} 的 choices 只给数字项用")
            check(len(values) == len(set(values)), f"规格：{key} 的 choices 重复", values)
            check(default in values, f"规格：{key} 的默认值不在 choices 里", values)
            check(all(type(v) is int and s["min"] <= v <= s["max"] for v in values),
                  f"规格：{key} 的 choices 不在 {s['min']}–{s['max']} 里", values)
            check(all(c.get("name") for c in s["choices"]) and s.get("title") and s.get("info"),
                  f"规格：{key} 的 choices 要有 name，还要写 title、info")
        for end, presets in s.get("presets", {}).items():
            check(end in s["platforms"], f"规格：{key} 的 presets 写了没有这一项的 {end}")
            check(default_for(s, end) in presets, f"规格：{key} 在 {end} 的默认值不在 presets 里", presets)
        missing = s.get("whenMissing")
        if missing:
            # ifTrue 可以是已经不是设置项的旧键（只读不写，见 legacy_key）
            other = missing.get("ifNonEmpty") or missing.get("ifTrue")
            check(other in by_key or other == legacy_key(s, by_key), f"规格：{key} 的 whenMissing 用了没有的键 {other}")
            for case in missing["cases"]:
                check(set(case["file"]) <= set(by_key) | {legacy_key(s, by_key)}, f"规格：{key} 的例子用了没有的键",
                      case["file"])
    for case in spec["readRules"]["cases"]["items"]:
        for key in case["expect"]:
            check(key in by_key and set(by_key[key]["platforms"]) == set(ENDS),
                  f"规格：readRules.cases 用的 {key} 不是三端都有的键")
    policy = spec["readRules"]["outOfRange"]
    for end in ENDS:
        check(policy.get(end) in ("default", "keep", "clamp"), f"规格：readRules.outOfRange 没写 {end} 或写错了",
              policy.get(end))

    # 设置窗口的强调色：三端都用规格里的颜色，这里核对对比度（见 SettingsAccent 的 note）
    for v in by_key["SettingsAccent"]["values"]:
        colors = [v.get(name, "") for name in ("color", "strong", "light")]
        if not check(all(re.fullmatch(r"#[0-9A-F]{6}", c) for c in colors),
                     f"规格：强调色 {v['value']} 的颜色要写成 #RRGGBB（大写）", colors):
            continue
        check(contrast(v["strong"], "#FFFFFF") >= 4.7, f"规格：强调色 {v['value']} 的 strong 和白字的对比度不到 4.7",
              f"{contrast(v['strong'], '#FFFFFF'):.2f}")
        check(contrast(v["strong"], "#FAFAFB") >= 4.5,
              f"规格：强调色 {v['value']} 的 strong 在浅色窗口底色上当字用，对比度不到 4.5", f"{contrast(v['strong'], '#FAFAFB'):.2f}")
        check(contrast(v["light"], "#353539") >= 4.5,
              f"规格：强调色 {v['value']} 的 light 在深色卡片上当字用，对比度不到 4.5", f"{contrast(v['light'], '#353539'):.2f}")

    bili = spec["translateBilibili"]
    ids = [m["id"] for m in bili["models"]]
    check(ids and ids[0] == bili["default"], "规格：translateBilibili.default 要是 models 的第一个", ids)
    check(len(ids) == len(set(ids)), "规格：translateBilibili.models 重复", ids)
    check(all(i.lower().startswith(bili["modelPrefix"].lower()) for i in ids),
          "规格：translateBilibili 的模型名都要以 modelPrefix 开头", ids)
    same(re.sub(r"^https?://", "", bili["url"]).split("/")[0], bili["host"], "规格：translateBilibili.host 是 url 的主机")
    check(all(bili["url"] != p["url"] for p in spec["translateProviders"]["items"]),
          "规格：哔哩哔哩不在 translateProviders 里（它是单独的引擎）")
    same([e["id"] for e in spec["translateEngines"]["items"]], ["local", "bilibili", "custom"],
         "规格：translateEngines 的顺序")

    pattern = re.compile(re.escape(pack["assetPrefix"]) + "(" + pack["versionPattern"] + ")"
                         + re.escape(pack["assetSuffix"]))
    for case in pack["cases"]["assetVersion"]:
        match = pattern.fullmatch(case["name"])
        same(match.group(1) if match else None, case["version"], f"规格：词库包文件名的例子 {case['name']}")
    check(len(pack["files"]) == len(set(pack["files"])), "规格：词库包的 files 重复")


# ---------------------------------------------------------------- 设置界面（Python）


def fake_gi():
    """够设置界面在顶层用的假 gi：require_version 什么都不做，gi.repository 里随便取的名字都是空类，
    调用它（比如类体里的 GObject.Property(...)）也只得到一个空类。"""

    class Stub(type):
        def __getattr__(cls, name):
            return Stub(name, (), {})

        def __call__(cls, *_args, **_kwargs):
            return Stub(cls.__name__, (), {})

    gi = types.ModuleType("gi")
    gi.require_version = lambda *_args: None
    repository = types.ModuleType("gi.repository")
    repository.__getattr__ = lambda name: Stub(name, (), {})
    gi.repository = repository
    return {"gi": gi, "gi.repository": repository}


def import_settings(home):
    for name in ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME"):
        os.environ[name] = os.path.join(home, name.lower())
    for name in ("WOWTYPE_CUSTOM", "WOWTYPE_LOG", "WOWTYPE_TRANSLATE_KEY_FILE", "WOWTYPE_VOICE_MODEL_DIR"):
        os.environ.pop(name, None)
    saved = {name: sys.modules.get(name) for name in ("gi", "gi.repository")}
    sys.modules.update(fake_gi())
    try:
        loader = importlib.machinery.SourceFileLoader("wowtype_config", str(SETTINGS))
        module = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
        loader.exec_module(module)
    finally:
        for name, value in saved.items():
            if value is None:
                sys.modules.pop(name, None)
            else:
                sys.modules[name] = value
    return module


def read_settings_config(wc, home, entries):
    """把 entries 写成 wowtype.conf，用设置界面的 read_config 读回来。"""
    path = os.path.join(home, "wowtype.conf")
    with open(path, "w", encoding="utf-8") as f:
        f.write("".join(f"{key}={value}\n" for key, value in entries.items()))
    saved, wc.CONFIG_FILE = wc.CONFIG_FILE, path
    try:
        return wc.read_config()[0]
    finally:
        wc.CONFIG_FILE = saved


def read_settings_text(wc, home, text):
    """把 text 原样写成 wowtype.conf，用设置界面的 read_config 读回来。"""
    path = os.path.join(home, "wowtype.conf")
    with open(path, "w", encoding="utf-8") as f:
        f.write(text + "\n")
    saved, wc.CONFIG_FILE = wc.CONFIG_FILE, path
    try:
        return wc.read_config()[0]
    finally:
        wc.CONFIG_FILE = saved


def legacy_key(setting, by_key):
    """whenMissing 的 ifTrue 指着的、已经不是设置项的旧键（InputScheme 的 Shuangpin）：只在读文件时看，
    存盘时不写，文件里有的那行下次存盘就没了。没有是 None。"""
    other = (setting.get("whenMissing") or {}).get("ifTrue")
    return other if other and other not in by_key else None


def check_settings(spec, wc, home):
    mine = settings_for(spec, "settings")
    same_set(wc.DEFAULTS, [s["key"] for s in mine], "设置界面：DEFAULTS 的键")
    for s in mine:
        if s["key"] in wc.DEFAULTS:
            same(wc.DEFAULTS[s["key"]], default_for(s, "settings"), f"设置界面：{s['key']} 的默认值")
    by_key = {s["key"]: s for s in spec["settings"]}

    same(wc.RANGES, {s["key"]: (s["min"], s["max"]) for s in mine if s["type"] == "int"}, "设置界面：RANGES")
    same({k: list(v) for k, v in wc.ENUMS.items()},
         {s["key"]: enum_values(s) for s in mine if s["type"] == "enum"}, "设置界面：ENUMS（读文件时认的枚举值）")

    # 选项列表：(界面上的名字, 存盘名)
    lists = {"InputScheme": wc.INPUT_SCHEMES, "ShuangpinScheme": wc.SHUANGPIN_SCHEMES,
             "ShiftToggle": wc.SHIFT_TOGGLES,
             "InitialMode": wc.INITIAL_MODES, "WindowMode": wc.WINDOW_MODES,
             "ArrowKeys": wc.ARROW_KEYS,
             "SelectKeys": wc.SELECT_KEYS, "PunctuationStyle": wc.PUNCT_STYLES}
    for key, items in lists.items():
        same([value for _label, value in items], enum_values(by_key[key]), f"设置界面：{key} 的选项")
    same([(label, value) for label, value in wc.SHUANGPIN_SCHEMES],
         [(v["name"], v["value"]) for v in by_key["ShuangpinScheme"]["values"]], "设置界面：双拼方案的名字")
    same([(label, value) for label, value in wc.INPUT_SCHEMES],
         [(v["name"], v["value"]) for v in by_key["InputScheme"]["values"]], "设置界面：输入方案的名字")
    same([tuple(item) for item in wc.SETTINGS_THEMES],
         [(v["name"], v["value"]) for v in by_key["SettingsTheme"]["values"]], "设置界面：SETTINGS_THEMES")
    same([tuple(item) for item in wc.SETTINGS_ACCENTS], accent_rows(by_key["SettingsAccent"]),
         "设置界面：SETTINGS_ACCENTS")
    same([tuple(item) for item in wc.TRANSLATE_LANGUAGES],
         [(v["name"], v["value"], v["english"]) for v in by_key["TranslateTarget"]["values"]],
         "设置界面：TRANSLATE_LANGUAGES")
    same([tuple(item) for item in wc.TRANSLATE_PROVIDERS],
         [(p["name"], p["url"], p["model"]) for p in spec["translateProviders"]["items"]],
         "设置界面：TRANSLATE_PROVIDERS")
    same([tuple(item) for item in wc.TRANSLATE_ENGINES],
         [(e["name"], e["note"]) for e in spec["translateEngines"]["items"]], "设置界面：TRANSLATE_ENGINES")
    bili = spec["translateBilibili"]
    same((wc.BILIBILI_URL, wc.BILIBILI_HOST, wc.BILIBILI_MODEL_PREFIX, wc.BILIBILI_DEFAULT_MODEL, wc.BILIBILI_PRIVACY),
         (bili["url"], bili["host"], bili["modelPrefix"], bili["default"], bili["privacy"]),
         "设置界面：哔哩哔哩的地址、主机、模型前缀、默认模型、隐私说明")
    same([tuple(m) for m in wc.BILIBILI_MODELS], [(m["id"], m["name"]) for m in bili["models"]],
         "设置界面：BILIBILI_MODELS")
    same(wc.INDEX_TRANSLATE_PROMPT, {k: v for k, v in bili["prompt"].items() if not k.startswith("$")},
         "设置界面：INDEX_TRANSLATE_PROMPT")
    for item in by_key["TranslateTarget"]["values"]:
        for formal in (False, True):
            for source, chinese in ((" 明天见\n", True), ("See you", False)):
                same(wc.index_translate_prompt(source, item["value"], formal),
                     index_prompt(bili["prompt"], source, item["name"] if chinese else None, formal),
                     f"设置界面：Index-Translate 提示词（{item['value']}，{'正式' if formal else '聊天'}，{source.strip()}）")
    engine = {"local": 0, "bilibili": 1, "custom": 2}
    for values, want in (({"TranslateLocal": "True", "TranslateBaseUrl": bili["url"]}, "local"),
                         ({"TranslateLocal": "False", "TranslateBaseUrl": bili["url"]}, "bilibili"),
                         ({"TranslateLocal": "False", "TranslateBaseUrl": bili["url"] + "/chat/completions"}, "bilibili"),
                         ({"TranslateLocal": "False", "TranslateBaseUrl": "https://api.deepseek.com/v1"}, "custom"),
                         ({"TranslateLocal": "False", "TranslateBaseUrl": ""}, "custom")):
        same(wc.translate_engine(values), engine[want], f"设置界面：{values} 是「{want}」")
    same([value for _label, value in wc.EMOJI_KEYS], by_key["EmojiKey"]["presets"]["settings"],
         "设置界面：EMOJI_KEYS")
    silence = by_key["VoiceSilenceTimeout"]
    same(wc.VOICE_SILENCE_TIMEOUTS, [(c["name"], str(c["value"])) for c in silence["choices"]],
         "设置界面：VoiceSilenceTimeout 的选项（VOICE_SILENCE_TIMEOUTS）")
    same((wc.VOICE_SILENCE_TITLE, wc.VOICE_SILENCE_INFO), (silence["title"], silence["info"]),
         "设置界面：VoiceSilenceTimeout 的名字和说明")
    same([value for _label, value in wc.VOICE_KEYS], by_key["VoiceKey"]["presets"]["settings"],
         "设置界面：VOICE_KEYS")
    same([value for _label, value in wc.TOGGLE_KEYS], by_key["ToggleKey"]["presets"]["settings"],
         "设置界面：TOGGLE_KEYS")
    same([value for _label, value in wc.TRANSLATE_MODE_KEYS], by_key["TranslateModeKey"]["presets"]["settings"],
         "设置界面：TRANSLATE_MODE_KEYS")
    same_set([key for key, _label in wc.FUZZY_GROUPS], [s["key"] for s in mine if s.get("group") == "fuzzy"],
             "设置界面：FUZZY_GROUPS 的键")

    # 读文件的规矩
    for s in mine:
        missing = s.get("whenMissing")
        for case in missing["cases"] if missing else ():
            same(read_settings_config(wc, home, case["file"])[s["key"]], file_text(case["expect"]),
                 f"设置界面：文件是 {case['file']} 时 {s['key']}")
    # 旧键：从只有旧键的文件读进来，改成每一个可选值（包括默认值）存盘，读回来是改的值，文件里没有旧键了
    for s in mine:
        old = legacy_key(s, by_key)
        if not old:
            continue
        for value in enum_values(s):
            path = os.path.join(home, "wowtype.conf")
            with open(path, "w", encoding="utf-8") as f:
                f.write(f"{old}=True\n")
            saved, wc.CONFIG_FILE = wc.CONFIG_FILE, path
            try:
                values, extra = wc.read_config()
                wc.write_config({**values, s["key"]: value}, extra)
                again = wc.read_config()[0]
            finally:
                wc.CONFIG_FILE = saved
            with open(path, encoding="utf-8") as f:
                text = f.read()
            same(again[s["key"]], value, f"设置界面：{old}=True 的文件改成 {s['key']}={value} 存盘再读")
            check(f"\n{old}=" not in "\n" + text, f"设置界面：存盘以后文件里还有旧键 {old}", text)
    policy = spec["readRules"]["outOfRange"]["settings"]
    for s in mine:
        if s["type"] == "int":
            for raw, expect in out_of_range_cases(s, policy, "settings"):
                same(read_settings_config(wc, home, {s["key"]: raw})[s["key"]], expect,
                     f"设置界面：{s['key']}={raw} 读出来")
    for case in spec["readRules"]["cases"]["items"]:
        values = read_settings_text(wc, home, case["file"])
        for key, expect in case["expect"].items():
            same(values[key], expect, f"设置界面：文件是 {case['file']!r} 时 {key}")


def make_tar(path, members):
    """members：[(名字, 内容)]，内容是 None 的是目录。"""
    with tarfile.open(path, "w:gz") as tar:
        for name, text in members:
            info = tarfile.TarInfo(name)
            if text is None:
                info.type = tarfile.DIRTYPE
                tar.addfile(info)
            else:
                data = text.encode("utf-8")
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))


def try_install(wc, tmp, members, minimum):
    """装一个词库包，装上了返回 True，被拒（ValueError）返回 False。"""
    pack = os.path.join(tmp, "pack.tar.gz")
    folder = os.path.join(tmp, "dicts", "wanxiang")
    make_tar(pack, members)
    saved, wc.LEXICON_PACK_MIN = wc.LEXICON_PACK_MIN, minimum
    try:
        wc.install_lexicon_pack(pack, folder)
        return True
    except ValueError:
        return False
    finally:
        wc.LEXICON_PACK_MIN = saved
        wc.remove_lexicon_pack(folder)


class FakeResponse(io.BytesIO):
    def __init__(self, data):
        super().__init__(data)
        self.headers = {"Content-Length": str(len(data))}


def check_settings_lexicon(pack, wc, home):
    same(sorted(wc.LEXICON_PACK_FILES), sorted(pack["files"]), "设置界面：LEXICON_PACK_FILES")
    same(wc.LEXICON_PACK_MIN, pack["minEntries"], "设置界面：LEXICON_PACK_MIN")
    same(os.path.relpath(wc.WANXIANG_DIR, wc.DATA_DIR), pack["folder"], "设置界面：WANXIANG_DIR（在数据目录下）")
    same(os.path.relpath(wc.WANXIANG_LEXICON, wc.WANXIANG_DIR), "lexicon.tsv", "设置界面：WANXIANG_LEXICON")
    cases = pack["cases"]
    for case in cases["assetVersion"]:
        match = wc.WANXIANG_ASSET.fullmatch(case["name"])
        same(match.group(1) if match else None, case["version"], f"设置界面：词库包文件名 {case['name']}")
    for case in cases["pickAsset"]:
        found = wc.find_lexicon_asset({"assets": {name: f"u{i}" for i, name in enumerate(case["names"])}})
        same(found[0] if found else None, case["pick"], f"设置界面：从 {case['names']} 里挑词库包")

    # SHA256SUMS：download_updates 从假的发布页下（要的文件内容是空的）
    wanted = cases["checksums"]["wanted"]
    served = {}

    def fake_open(url, timeout=30):
        return FakeResponse(served[url])
    saved = wc.http_open, wc.UPDATE_DIR
    wc.http_open, wc.UPDATE_DIR = fake_open, os.path.join(home, "updates")
    try:
        for case in cases["checksums"]["cases"]:
            served.clear()
            served.update({"sums": case["text"].encode("utf-8"), "pack": b""})
            release = {"tag": "v9.9.9", "assets": {pack["checksumFile"]: "sums", wanted: "pack"}}
            try:
                wc.download_updates(release, [(wanted, "pack")], lambda *_a: None)
                ok = True
            except RuntimeError:
                ok = False
            same(ok, case["digest"] is not None, f"设置界面：{pack['checksumFile']} 是 {case['text']!r} 时装不装")
        # 能装的例子取到的校验值都得是空文件的（下载下来的内容是空的）
        for case in cases["checksums"]["cases"]:
            check(case["digest"] in (None, hashlib.sha256(b"").hexdigest()),
                  "规格：checksums 能装的例子的 digest 要是空文件的 SHA256", case["digest"])
    finally:
        wc.http_open, wc.UPDATE_DIR = saved

    with tempfile.TemporaryDirectory(dir=home) as tmp:
        lexicon = "ni\t你\n"
        for case in cases["entryName"]:
            raw, name = case["raw"], case["name"]
            members = [] if name == "lexicon.tsv" else [("lexicon.tsv", lexicon)]
            members.append((raw, None if name == "" else lexicon))
            same(try_install(wc, tmp, members, 1), name is not None, f"设置界面：包里有 {raw!r} 时装不装")
        for case in cases["lexicon"]["cases"]:
            members = [("lexicon.tsv", case["text"]), ("VERSION", "1\n")]
            count = case["count"]
            if count is None:
                same(try_install(wc, tmp, members, 0), False, f"设置界面：lexicon.tsv 是 {case['text']!r} 时不装")
            else:
                same(try_install(wc, tmp, members, count), True,
                     f"设置界面：lexicon.tsv 是 {case['text']!r} 时有 {count} 条（最少 {count} 条能装）")
                same(try_install(wc, tmp, members, count + 1), False,
                     f"设置界面：lexicon.tsv 是 {case['text']!r} 时有 {count} 条（最少 {count + 1} 条不装）")
        for i, case in enumerate(cases["installedVersion"]):
            folder = os.path.join(tmp, f"installed{i}")
            os.makedirs(folder)
            for name, text in case["files"].items():
                with open(os.path.join(folder, name), "w", encoding="utf-8") as f:
                    f.write(text)
            same(wc.lexicon_pack_version(folder), case["version"], f"设置界面：装好的词库包 {case['files']} 的版本")


# ---------------------------------------------------------------- fcitx5 插件（C++）


def luminance(color):
    """WCAG 的相对亮度，color 是 #RRGGBB。"""
    def linear(c):
        c /= 255
        return c / 12.92 if c <= 0.04045 else ((c + 0.055) / 1.055) ** 2.4
    r, g, b = (linear(int(color[i:i + 2], 16)) for i in (1, 3, 5))
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def contrast(first, second):
    high, low = sorted((luminance(first), luminance(second)), reverse=True)
    return (high + 0.05) / (low + 0.05)


def accent_rows(setting):
    """强调色的表：(名字, 存盘名, color, strong, light)。"""
    return [(v["name"], v["value"], v["color"], v["strong"], v["light"]) for v in setting["values"]]


def run_dump(exe, *args):
    result = subprocess.run([exe, *args], capture_output=True, text=True, encoding="utf-8", timeout=30)
    if result.returncode != 0:
        raise SystemExit(f"{exe} {' '.join(args)} 失败：{result.stderr}")
    return json.loads(result.stdout)


def check_fcitx5(spec, exe):
    options = run_dump(exe)["options"]
    external = [o["key"] for o in options if o["type"] == "External"]
    same(external, spec["fcitx5External"]["keys"], "fcitx5：ExternalOption")
    options = {o["key"]: o for o in options if o["type"] != "External"}
    by_key = {s["key"]: s for s in spec["settings"]}
    mine = settings_for(spec, "fcitx5")
    same_set(options, [s["key"] for s in mine], "fcitx5：配置工具里列出的键")
    saved = run_dump(exe, "--load")["values"]
    same_set(saved, [s["key"] for s in mine], "fcitx5：WowtypeConfig 存盘的键")
    for s in mine:
        key = s["key"]
        option = options.get(key)
        if option is None:
            continue
        same(FCITX_TYPES.get(option["type"], option["type"]), s["type"], f"fcitx5：{key} 的类型")
        same(option["default"], default_for(s, "fcitx5"), f"fcitx5：{key} 的默认值")
        if s["type"] == "int":
            same((option["min"], option["max"]), (str(s["min"]), str(s["max"])), f"fcitx5：{key} 的范围")
        if s["type"] == "enum":
            same(option["values"], enum_values(s), f"fcitx5：{key} 的可选值")
            names = [v["name"] for v in s["values"] if isinstance(v, dict) and "name" in v]
            if names:
                same(option["names"], names, f"fcitx5：{key} 的可选值的名字（EnumI18n）")

    for s in mine:
        missing = s.get("whenMissing")
        for case in missing["cases"] if missing else ():
            values = run_dump(exe, "--load", *(f"{k}={v}" for k, v in case["file"].items()))["values"]
            same(values.get(s["key"]), file_text(case["expect"]), f"fcitx5：文件是 {case['file']} 时 {s['key']}")
            # 存盘的值（插件存盘只写 WowtypeConfig 里有的项）里没有旧键
            check(legacy_key(s, by_key) not in values, f"fcitx5：文件是 {case['file']} 时存盘还写了旧键 {legacy_key(s, by_key)}")
    policy = spec["readRules"]["outOfRange"]["fcitx5"]
    for s in mine:
        if s["type"] == "int":
            for raw, expect in out_of_range_cases(s, policy, "fcitx5"):
                values = run_dump(exe, "--load", f"{s['key']}={raw}")["values"]
                same(values.get(s["key"]), expect, f"fcitx5：{s['key']}={raw} 读出来")
    with tempfile.TemporaryDirectory() as tmp:
        path = os.path.join(tmp, "wowtype.conf")
        for case in spec["readRules"]["cases"]["items"]:
            with open(path, "w", encoding="utf-8") as f:
                f.write(case["file"] + "\n")
            values = run_dump(exe, "--load-file", path)["values"]
            for key, expect in case["expect"].items():
                same(values.get(key), expect, f"fcitx5：文件是 {case['file']!r} 时 {key}")


def check_windows(spec, exe):
    """Windows 外壳（windows/src/common/config.cpp）：键、类型、默认值、范围、可选值，按文件读出来的值。"""
    table = {s["key"]: s for s in run_dump(exe)["settings"]}
    mine = settings_for(spec, "windows")
    same([s["key"] for s in run_dump(exe)["settings"]], [s["key"] for s in mine], "windows：认的键（顺序同规格）")
    defaults = run_dump(exe, "--load")["values"]
    for s in mine:
        key = s["key"]
        ours = table.get(key)
        if ours is None:
            continue
        default = default_for(s, "windows")
        same(ours["type"], s["type"], f"windows：{key} 的类型")
        same(ours["default"], default, f"windows：{key} 的默认值（表里）")
        same(defaults.get(key), default, f"windows：{key} 的默认值（空文件读出来）")
        if s["type"] == "int":
            same((ours["min"], ours["max"]), (s["min"], s["max"]), f"windows：{key} 的范围")
        if s["type"] == "enum":
            same(ours["values"], enum_values(s), f"windows：{key} 的可选值")
            for v in enum_values(s):
                same(run_dump(exe, "--load", f"{key}={v}")["values"].get(key), v, f"windows：{key} 认 {v}")
        if s["type"] == "bool":
            for v in ("True", "False"):
                same(run_dump(exe, "--load", f"{key}={v}")["values"].get(key), v, f"windows：{key}={v}")
        if s["type"] == "key":
            same(run_dump(exe, "--load", f"{key}=")["values"].get(key), "", f"windows：{key} 留空是不用")
            shared = file_text(s["default"])
            if shared:
                # 别的端的默认值拷过来，读成 Windows 的默认值（EmojiKey：Super 当 Control，原因见规格的 why）
                same(run_dump(exe, "--load", f"{key}={shared}")["values"].get(key), default,
                     f"windows：{key} 写 {shared} 读出来")
        missing = s.get("whenMissing")
        for case in missing["cases"] if missing else ():
            values = run_dump(exe, "--load", *(f"{k}={v}" for k, v in case["file"].items()))["values"]
            same(values.get(key), file_text(case["expect"]), f"windows：文件是 {case['file']} 时 {key}")
    look = run_dump(exe, "--look")
    by_key = {s["key"]: s for s in spec["settings"]}
    # 语音页的「停顿多久自动结束」下拉框（common/config.h 的 voiceSilenceChoices），名字、说明和别的端一样
    silence = by_key["VoiceSilenceTimeout"]
    voice = run_dump(exe, "--voice")
    same([(c["value"], c["name"]) for c in voice["silenceChoices"]],
         [(c["value"], c["name"]) for c in silence["choices"]], "windows：VoiceSilenceTimeout 的选项")
    same((voice["silenceTitle"], voice["silenceInfo"]), (silence["title"], silence["info"]),
         "windows：VoiceSilenceTimeout 的名字和说明")
    same([(t["name"], t["value"]) for t in look["themes"]],
         [(v["name"], v["value"]) for v in by_key["SettingsTheme"]["values"]], "windows：设置窗口主题的名字")
    same([(a["name"], a["value"], a["color"], a["strong"], a["light"]) for a in look["accents"]],
         accent_rows(by_key["SettingsAccent"]), "windows：强调色的表（look::accents）")
    # 设置窗口「翻译」页的「翻译引擎」（windows/src/common/translate_engine）：选项、说明、哔哩哔哩的地址和模型，
    # 和按文件推出来的引擎都和设置界面的一样
    check("windows" in spec["translateEngines"]["platforms"], "规格：translateEngines 的 platforms 要有 windows")
    tr = run_dump(exe, "--translate")
    same([(e["id"], e["name"], e["note"]) for e in tr["engines"]],
         [(e["id"], e["name"], e["note"]) for e in spec["translateEngines"]["items"]], "windows：翻译引擎的选项和说明")
    bili = spec["translateBilibili"]
    same({k: tr["bilibili"][k] for k in ("url", "host", "modelPrefix", "default", "privacy")},
         {k: bili[k] for k in ("url", "host", "modelPrefix", "default", "privacy")},
         "windows：哔哩哔哩的地址、主机、模型前缀、默认模型、隐私说明")
    same([(m["id"], m["name"]) for m in tr["bilibili"]["models"]], [(m["id"], m["name"]) for m in bili["models"]],
         "windows：哔哩哔哩的模型")
    for values, want in (({"TranslateLocal": "True", "TranslateBaseUrl": bili["url"]}, "local"),
                         ({"TranslateLocal": "False", "TranslateBaseUrl": bili["url"]}, "bilibili"),
                         ({"TranslateLocal": "False", "TranslateBaseUrl": bili["url"] + "/chat/completions"}, "bilibili"),
                         ({"TranslateLocal": "False",
                           "TranslateBaseUrl": "HTTPS://Index-Translate.Bilibili.com/v1/chat/completions"}, "bilibili"),
                         ({"TranslateLocal": "False",
                           "TranslateBaseUrl": "https://index-translate.bilibili.com.example.org/v1"}, "custom"),
                         ({"TranslateLocal": "False", "TranslateBaseUrl": "https://api.deepseek.com/v1"}, "custom"),
                         ({"TranslateLocal": "False", "TranslateBaseUrl": ""}, "custom"),
                         ({"TranslateBaseUrl": bili["url"]}, "bilibili"),
                         ({}, "local")):
        same(run_dump(exe, "--engine", *(f"{k}={v}" for k, v in values.items()))["engine"], want,
             f"windows：{values} 是「{want}」")
    policy = spec["readRules"]["outOfRange"]["windows"]
    for s in mine:
        if s["type"] == "int":
            for raw, expect in out_of_range_cases(s, policy, "windows"):
                values = run_dump(exe, "--load", f"{s['key']}={raw}")["values"]
                same(values.get(s["key"]), expect, f"windows：{s['key']}={raw} 读出来")
    with tempfile.TemporaryDirectory() as tmp:
        path = os.path.join(tmp, "wowtype.conf")
        for case in spec["readRules"]["cases"]["items"]:
            with open(path, "w", encoding="utf-8", newline="\n") as f:
                f.write(case["file"] + "\n")
            values = run_dump(exe, "--load-file", path)["values"]
            for key, expect in case["expect"].items():
                same(values.get(key), expect, f"windows：文件是 {case['file']!r} 时 {key}")


def main(argv):
    # Windows 的控制台默认不是 UTF-8（cp1252 / cp936），中文的结果行会写不出来
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8")
    fcitx5 = windows = None
    if argv[:1] == ["--fcitx5"] and len(argv) == 2:
        fcitx5 = argv[1]
    elif argv[:1] == ["--windows"] and len(argv) == 2:
        windows = argv[1]
    elif argv:
        print(__doc__)
        return 2
    spec, pack = load_spec("config.json"), load_spec("lexicon-pack.json")
    check_spec(spec, pack)
    if windows:
        check_windows(spec, windows)
        ends = "规格、Windows 外壳"
    else:
        with tempfile.TemporaryDirectory() as home:
            wc = import_settings(home)
            check_settings(spec, wc, home)
            check_settings_lexicon(pack, wc, home)
        ends = "规格、设置界面"
    if fcitx5:
        check_fcitx5(spec, fcitx5)
        ends += "、fcitx5 插件"
    if failures:
        print(f"设置契约对不上（{len(failures)} 处）。改的是设置项就先改 spec/，再让各端跟上：\n")
        for failure in failures:
            print(f"FAIL {failure}")
        return 1
    print(f"config contract ok（{ends}，{checked} 项）")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
