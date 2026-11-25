// 导出 Windows 外壳认的设置项，给 scripts/test-config-contract.py --windows 拿 spec/config.json 对照。
//
//   wowtype-windows-config-dump                     设置项表：{"settings": [{key, type, default, min, max, values}]}
//   wowtype-windows-config-dump --load k=v ...      这几行当 wowtype.conf 读出来的值：{"values": {...}}
//   wowtype-windows-config-dump --load-file <path>  读这个文件
//   wowtype-windows-config-dump --look              设置窗口的主题、强调色：{"themes": [...], "accents": [...]}
//   wowtype-windows-config-dump --translate         「翻译引擎」的选项和哔哩哔哩的地址、模型：{"engines": [...], "bilibili": {...}}
//   wowtype-windows-config-dump --engine k=v ...    这几行当 wowtype.conf 读出来时选中的引擎：{"engine": "local"}
//   wowtype-windows-config-dump --voice             「停顿多久自动结束」的名字、说明、选项：{"silenceTitle": ..., "silenceChoices": [...]}

#include <cstdio>
#include <fstream>
#include <iterator>
#include <sstream>
#include <string>

#include "config.h"
#include "look.h"
#include "translate_engine.h"

using namespace wowtype;

namespace {

std::string json(const std::string &s) {
    std::string out = "\"";
    for (const char c : s) {
        switch (c) {
        case '"': out += "\\\""; break;
        case '\\': out += "\\\\"; break;
        case '\n': out += "\\n"; break;
        case '\r': out += "\\r"; break;
        case '\t': out += "\\t"; break;
        default:
            if (static_cast<unsigned char>(c) < 0x20) {
                char buf[8];
                std::snprintf(buf, sizeof buf, "\\u%04x", c);
                out += buf;
            } else {
                out += c;
            }
        }
    }
    return out + "\"";
}

const char *typeName(SettingType t) {
    switch (t) {
    case SettingType::Bool: return "bool";
    case SettingType::Int: return "int";
    case SettingType::Enum: return "enum";
    case SettingType::String: return "string";
    case SettingType::Key: return "key";
    }
    return "?";
}

void dumpValues(const Config &config) {
    std::string out = "{\"values\": {";
    bool first = true;
    for (const auto &def : settingDefs()) {
        out += (first ? "" : ", ") + json(def.key) + ": " + json(config.value(def.key));
        first = false;
    }
    std::printf("%s}}\n", out.c_str());
}

} // namespace

int main(int argc, char **argv) {
    const std::string mode = argc > 1 ? argv[1] : "";
    if (mode.empty()) {
        std::string out = "{\"settings\": [";
        bool first = true;
        for (const auto &def : settingDefs()) {
            out += first ? "" : ", ";
            first = false;
            out += "{\"key\": " + json(def.key) + ", \"type\": " + json(typeName(def.type)) +
                   ", \"default\": " + json(def.defaultValue);
            if (def.type == SettingType::Int) {
                out += ", \"min\": " + std::to_string(def.min) + ", \"max\": " + std::to_string(def.max);
            }
            if (def.type == SettingType::Enum) {
                out += ", \"values\": [";
                for (std::size_t i = 0; i < def.values.size(); ++i) {
                    out += (i ? ", " : "") + json(def.values[i]);
                }
                out += "]";
            }
            out += "}";
        }
        std::printf("%s]}\n", out.c_str());
        return 0;
    }
    if (mode == "--load") {
        std::string text;
        for (int i = 2; i < argc; ++i) {
            text += std::string(argv[i]) + "\n";
        }
        dumpValues(Config::parse(text));
        return 0;
    }
    if (mode == "--look") {
        std::string out = "{\"themes\": [";
        for (std::size_t i = 0; i < look::themes().size(); ++i) {
            const auto &t = look::themes()[i];
            out += std::string(i ? ", " : "") + "{\"value\": " + json(t.value) + ", \"name\": " + json(t.name) + "}";
        }
        out += "], \"accents\": [";
        for (std::size_t i = 0; i < look::accents().size(); ++i) {
            const auto &a = look::accents()[i];
            out += std::string(i ? ", " : "") + "{\"value\": " + json(a.value) + ", \"name\": " + json(a.name) +
                   ", \"color\": " + json(look::hex(a.color)) + ", \"strong\": " + json(look::hex(a.strong)) +
                   ", \"light\": " + json(look::hex(a.light)) + "}";
        }
        std::printf("%s]}\n", out.c_str());
        return 0;
    }
    if (mode == "--translate") {
        std::string out = "{\"engines\": [";
        for (std::size_t i = 0; i < translate::engines().size(); ++i) {
            const auto &e = translate::engines()[i];
            out += std::string(i ? ", " : "") + "{\"id\": " + json(e.id) + ", \"name\": " + json(e.name) +
                   ", \"note\": " + json(e.note) + "}";
        }
        out += "], \"bilibili\": {\"url\": " + json(translate::kBilibiliUrl) + ", \"host\": " +
               json(translate::kBilibiliHost) + ", \"modelPrefix\": " + json(translate::kBilibiliModelPrefix) +
               ", \"default\": " + json(translate::kBilibiliDefaultModel) + ", \"privacy\": " +
               json(translate::kBilibiliPrivacy) + ", \"models\": [";
        for (std::size_t i = 0; i < translate::bilibiliModels().size(); ++i) {
            const auto &m = translate::bilibiliModels()[i];
            out += std::string(i ? ", " : "") + "{\"id\": " + json(m.id) + ", \"name\": " + json(m.name) + "}";
        }
        std::printf("%s]}}\n", out.c_str());
        return 0;
    }
    if (mode == "--voice") {
        std::string out = "{\"silenceTitle\": " + json(kVoiceSilenceTitle) + ", \"silenceInfo\": " +
                          json(kVoiceSilenceInfo) + ", \"silenceChoices\": [";
        for (std::size_t i = 0; i < voiceSilenceChoices().size(); ++i) {
            const auto &c = voiceSilenceChoices()[i];
            out += std::string(i ? ", " : "") + "{\"value\": " + std::to_string(c.value) + ", \"name\": " +
                   json(c.name) + "}";
        }
        std::printf("%s]}\n", out.c_str());
        return 0;
    }
    if (mode == "--engine") {
        std::string text;
        for (int i = 2; i < argc; ++i) {
            text += std::string(argv[i]) + "\n";
        }
        const auto engine = translate::engineOf(Config::parse(text));
        std::printf("{\"engine\": %s}\n", json(translate::engines()[static_cast<std::size_t>(engine)].id).c_str());
        return 0;
    }
    if (mode == "--load-file" && argc == 3) {
        std::ifstream in(argv[2], std::ios::binary);
        if (!in) {
            std::fprintf(stderr, "读不了 %s\n", argv[2]);
            return 1;
        }
        const std::string text((std::istreambuf_iterator<char>(in)), std::istreambuf_iterator<char>());
        dumpValues(Config::parse(text));
        return 0;
    }
    std::fprintf(stderr, "用法：wowtype-windows-config-dump [--load k=v ... | --load-file <path> | --look | --translate | --engine k=v ...]\n");
    return 2;
}
