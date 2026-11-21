#include "voice.h"

#include <algorithm>
#include <filesystem>
#include <map>

#include "composer.h"
#include "keys.h"

namespace wowtype {

namespace {

constexpr uint32_t kVkLwin = 0x5B;
constexpr uint32_t kVkRwin = 0x5C;
constexpr uint32_t kVkF1 = 0x70;

/// 模型和词表在不在（和 voice/src/wtlocal/download.rs 的 is_ready 一样：有 .xnet 和 dict.decoder.utf8.txt）。
bool defaultModelReady(const std::string &dir) {
    namespace fs = std::filesystem;
    std::error_code error;
    const fs::path path = fs::u8path(dir);
    if (!fs::is_regular_file(path / "dict.decoder.utf8.txt", error)) {
        return false;
    }
    for (const auto &entry : fs::directory_iterator(path, error)) {
        if (entry.path().extension() == ".xnet") {
            return true;
        }
    }
    return false;
}

} // namespace

std::optional<VoiceKey> VoiceKey::parse(const std::string &name) {
    static const std::map<std::string, VoiceKey> table = [] {
        std::map<std::string, VoiceKey> t;
        t["Alt_R"] = VoiceKey{vk::menu, true, true, "Alt_R"};
        t["Alt_L"] = VoiceKey{vk::menu, false, true, "Alt_L"};
        t["Control_R"] = VoiceKey{vk::control, true, true, "Control_R"};
        t["Control_L"] = VoiceKey{vk::control, false, true, "Control_L"};
        t["Super_R"] = VoiceKey{kVkRwin, true, true, "Super_R"};
        t["Super_L"] = VoiceKey{kVkLwin, false, true, "Super_L"};
        for (uint32_t i = 1; i <= 12; ++i) {
            const std::string f = "F" + std::to_string(i);
            t[f] = VoiceKey{kVkF1 + i - 1, false, false, f};
        }
        return t;
    }();
    const auto it = table.find(name);
    if (it == table.end()) {
        return std::nullopt;
    }
    return it->second;
}

bool VoiceKey::matches(uint32_t key, bool isRight) const {
    // 消息里也可能直接给分左右的键码（VK_RMENU、VK_LCONTROL……）
    switch (key) {
    case vk::lmenu: return virtualKey == vk::menu && !right;
    case vk::rmenu: return virtualKey == vk::menu && right;
    case vk::lcontrol: return virtualKey == vk::control && !right;
    case vk::rcontrol: return virtualKey == vk::control && right;
    default: break;
    }
    if (key != virtualKey) {
        return false;
    }
    if (virtualKey == vk::menu || virtualKey == vk::control) {
        return isRight == right;
    }
    return true;
}

std::string voiceWave(const std::vector<int> &levels) {
    static const char *const bars[] = {"▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"};
    const std::size_t shown = std::min<std::size_t>(levels.size(), 6);
    std::string out;
    for (std::size_t i = shown; i < 6; ++i) {
        out += bars[0];
    }
    for (std::size_t i = levels.size() - shown; i < levels.size(); ++i) {
        out += bars[std::clamp(levels[i], 0, 100) * 7 / 100];
    }
    return out;
}

VoiceInput::~VoiceInput() {
    if (handle_) {
        wowtype_voice_free(handle_);
    }
}

uint64_t VoiceInput::now() const { return clock ? clock() : monotonicMs(); }

bool VoiceInput::ready() const { return modelReady ? modelReady(modelDir_) : defaultModelReady(modelDir_); }

void VoiceInput::configure(bool enabled, const std::string &keyName, bool copyResult, const std::string &modelDir,
                           int silenceMs) {
    const std::optional<VoiceKey> wanted = enabled ? VoiceKey::parse(keyName) : std::nullopt;
    if (wanted != key_) {
        if (showing_) {
            cancel();
        }
        keyDown_ = false;
    }
    key_ = wanted;
    copyResult_ = copyResult;
    modelDir_ = modelDir;
    if (key_ && !handle_) {
        // 核心不带语音编的时候是 NULL，这时语音键什么也不做
        handle_ = wowtype_voice_new(modelDir.c_str());
    }
    if (handle_) {
        // 点按说话的静音超时（#285）：0 不自动结束，再点一下语音键结束
        wowtype_voice_set_silence_timeout(handle_, static_cast<unsigned>(silenceMs < 0 ? 0 : silenceMs));
    }
}

bool VoiceInput::press(bool down, bool repeat, bool composing, VoiceHost *host) {
    if (!handle_ || !key_ || !host) {
        return false;
    }
    if (!ready()) {
        // 还没放模型：不碰麦克风，只提示（提示文字是核心给的）
        if (down) {
            if (composing && !showing_) {
                return false;
            }
            pendingHint_ = true;
            // 修饰键松开时再提示；F 键这类按下就提示
            if (key_->modifier) {
                return false;
            }
        } else if (!pendingHint_) {
            return false;
        }
        pendingHint_ = false;
        if (showing_ && host_ != host) {
            cancel();
        }
        host_ = host;
        showing_ = true;
        holdUntil_ = 0;
        wowtype_voice_key_down(handle_);
        wowtype_voice_key_up(handle_);
        poll();
        if (onStartPolling) {
            onStartPolling();
        }
        return true;
    }
    if (!down) {
        if (!keyDown_) {
            return false;
        }
        keyDown_ = false;
        if (showing_ && host_ == host) {
            wowtype_voice_key_up(handle_);
            poll();
        }
        return true;
    }
    if (keyDown_ || repeat) {
        return keyDown_; // 按住时的自动重复
    }
    if (composing && !showing_) {
        return false; // 正在打拼音时不抢这个键
    }
    if (showing_ && host_ != host) {
        cancel();
    }
    keyDown_ = true;
    if (!showing_) {
        host_ = host;
        showing_ = true;
        levels_.clear();
        frame_ = 0;
        holdUntil_ = 0;
    }
    wowtype_voice_key_down(handle_);
    poll();
    if (onStartPolling) {
        onStartPolling();
    }
    return true;
}

bool VoiceInput::otherKey(bool escape, VoiceHost *host) {
    pendingHint_ = false;
    if (!showing_ || host_ != host) {
        return false;
    }
    if (escape) {
        cancel();
        return true;
    }
    // 说话途中打别的键：不要这次的语音，键照常处理
    cancel();
    return false;
}

void VoiceInput::cancel(VoiceHost *host) {
    if (host_ == host) {
        cancel();
    }
}

void VoiceInput::cancel() {
    if (handle_) {
        wowtype_voice_cancel(handle_);
    }
    finish();
}

bool VoiceInput::poll() {
    if (!handle_ || !showing_) {
        return false;
    }
    if (!host_) {
        cancel();
        return false;
    }
    const int phase = wowtype_voice_poll(handle_);
    const char *raw = wowtype_voice_text(handle_);
    const std::string text = raw ? raw : "";
    const uint64_t t = now();
    switch (phase) {
    case WOWTYPE_VOICE_LISTENING:
        levels_.push_back(wowtype_voice_level(handle_));
        if (levels_.size() > 6) {
            levels_.erase(levels_.begin());
        }
        host_->voiceShow("🎙 " + voiceWave(levels_), text);
        return true;
    case WOWTYPE_VOICE_RECOGNIZING: {
        static const char *const dots[] = {"·    ", "··   ", "···  ", "···· ", "·····"};
        host_->voiceShow(std::string("🎙 ") + dots[(frame_ / 3) % 5], text);
        ++frame_;
        return true;
    }
    case WOWTYPE_VOICE_RESULT: {
        VoiceHost *host = host_;
        finish();
        host->voiceCommit(text);
        if (copyResult_ && !text.empty() && copyToClipboard) {
            copyToClipboard(text);
        }
        return false;
    }
    case WOWTYPE_VOICE_ERROR: {
        host_->voiceShow("🎙 " + text, "");
        // 没放模型的提示要用户去操作，多留一会儿
        const bool missing = wowtype_voice_model_state(handle_) == WOWTYPE_VOICE_MODEL_MISSING;
        holdUntil_ = t + (missing ? 4000 : 1500);
        return true;
    }
    default:
        if (t < holdUntil_) {
            return true;
        }
        finish();
        return false;
    }
}

void VoiceInput::finish() {
    keyDown_ = false;
    if (showing_) {
        showing_ = false;
        if (host_) {
            host_->voiceShow(std::nullopt, "");
        }
    }
    host_ = nullptr;
}

} // namespace wowtype
