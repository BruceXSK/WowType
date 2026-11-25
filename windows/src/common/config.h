#pragma once

// 设置：%LOCALAPPDATA%\WowType\wowtype.conf（WOWTYPE_DATA_DIR 可以换地方）。
//
// 键名、取值、默认值、读法和 Linux 的 ~/.config/fcitx5/conf/wowtype.conf、macOS 的 wowtype.conf 一样，
// 一份配置三端都能用。以 spec/config.json 为准（设置契约，docs/CONFIG.md）：这里的表要和规格里
// platforms 带 windows 的项一一对上，scripts/test-config-contract.py --windows 对照（windows/tests/config_dump.cpp）。
// 文件不存在、少写了哪一项就用默认值；不认识的键不管。只有 EmojiKey 的默认值不一样（Control+period，见 Shortcut）。

#include <cstdint>
#include <map>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

#include "keys.h"

namespace wowtype {

enum class SettingType { Bool, Int, Enum, String, Key };

struct SettingDef {
    const char *key;
    SettingType type;
    const char *defaultValue; // 写进文件的样子（True、9、Horizontal、Control+period）
    int min = 0;
    int max = 0;
    std::vector<std::string> values; // 枚举的可选值
};

/// Windows 认的全部设置项，顺序同 spec/config.json。
const std::vector<SettingDef> &settingDefs();

/// 「停顿多久自动结束」（VoiceSilenceTimeout，毫秒，#285）：设置窗口语音页的下拉框。名字、选项、说明和
/// Linux、macOS 的设置一样（spec/config.json 的 title、info、choices，契约测试拿 config_dump --voice 对照）。
struct IntChoice {
    int value;
    const char *name;
};
inline constexpr char kVoiceSilenceTitle[] = "停顿多久自动结束";
inline constexpr char kVoiceSilenceInfo[] =
    "点按说话时没声音多久就自动结束；选「不自动结束」时再点一下语音键结束。"
    "一次最长录 2 分钟，到了也会结束。长按说话松开就结束，不受这一项影响。";
const std::vector<IntChoice> &voiceSilenceChoices();

class Config {
public:
    Config();

    /// 解析 fcitx5 的 INI 格式：一行一个 Key=Value，# 或 ; 开头是注释，值可以带双引号。
    /// 只认最外层的键（[分组] 下面的不是 WowType 的设置）。同一个键写了几次，算最后一次。
    static Config parse(std::string_view text);

    /// 这一项现在的值（写进文件的样子）；不认识的键是空串。
    std::string value(const std::string &key) const;
    /// 按文件里的写法改一项。不认识的键、认不出的值不管（保持原值）。
    void setValue(const std::string &key, std::string_view value);

    bool flag(const char *key) const { return value(key) == "True"; }
    int number(const char *key) const;
    bool is(const char *key, const char *enumValue) const { return value(key) == enumValue; }

    // ---- 给核心、外壳的值（和 macOS 的 WowtypeConfig 同名属性一样）
    bool horizontal() const { return is("CandidateLayout", "Horizontal"); }
    int pageSize() const { return number("PageSize"); }
    int candidateFontSize() const { return number("CandidateFontSize"); }
    uint32_t fuzzyGroups() const;
    uint32_t pageKeys() const;
    int selectKeys() const;
    int arrowKeys() const;
    int shiftToggle() const;
    uint32_t autoSpaceFlags() const;
    bool shuangpin() const { return is("InputScheme", "Shuangpin"); }
    bool wubi() const { return is("InputScheme", "Wubi86"); }
    bool halfwidthPunct() const { return is("PunctuationStyle", "Halfwidth"); }
    std::optional<Shortcut> emojiKey() const { return shortcut("EmojiKey"); }
    std::optional<Shortcut> toggleKey() const { return shortcut("ToggleKey"); }
    std::optional<Shortcut> translateModeKey() const { return shortcut("TranslateModeKey"); }

    /// 把设置写进配置文件的原文（existing 是 nullptr 或全是空白时从模板开始），返回新的原文。和 macOS 的
    /// WowtypeConfig.serialized(into:) 一样：只动认识的键，值读出来已经一样的那行原样不动，不一样的只换 = 后面的值；
    /// 注释、空行、不认识的键（检查更新这些 Windows 还没有的）、[分组] 原样留着，所以别的端拷过来的文件改完拿回去照样能用。
    /// 文件里没有、又不是默认值的键：有注释掉的那一行（# 键=默认值）就地换成生效的，没有就补在最后一个设置后面。
    /// 只删一种行：0.3.12 及以前的 Shuangpin 开关。
    std::string serialized(const std::string *existing) const;
    /// 窗口开着时文件在别处改了：只把自己相对 base 改过的键写进 current（current 是 nullptr 时从模板开始）。
    std::string merged(const Config &base, const std::string *current) const;
    /// 第一次打开设置时写出来的文件：说明 + 每项注释掉的默认值（# PageSize=9），一项都不生效。
    static std::string templateText();

    bool operator==(const Config &o) const { return values_ == o.values_; }
    bool operator!=(const Config &o) const { return !(*this == o); }

private:
    std::optional<Shortcut> shortcut(const char *key) const;
    std::map<std::string, std::string> values_;
};

/// 文件里没写 TranslateLocal 时它是什么：已经填好翻译接口（TranslateBaseUrl 去掉空白不是空的）的接着用接口（False），
/// 没填的用默认的本地模型。三端一样，见 spec/config.json 的 whenMissing。
bool impliedTranslateLocal(std::string_view baseUrl);

} // namespace wowtype
