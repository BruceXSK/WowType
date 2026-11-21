#pragma once

// 语音输入（和 Linux 插件的 handleVoiceKey / pollVoice、macOS 的 VoiceInput.swift 一样）：
// 按下语音键开始听，按住超过 300ms 松开即结束；更短算点按，继续听到静音（VoiceSilenceTimeout，默认 1.5 秒，
// 0 不自动结束）或再按一次（判断在核心里）。
// 正在听时编辑区里是识别到一半的字，候选框里是麦克风和跟着音量动的波形；出结果就上屏。
// 说话途中按 Esc 取消；按别的键也不要这次的语音，键照常处理。正在打拼音时不抢语音键。
//
// 识别在核心里（wowtype_voice_*，开麦克风、读模型、解码都在后台线程），外壳每 50 毫秒调一次 poll。
// 不依赖 windows.h：语音键用 Windows 的虚拟键码表示（就是整数），测试在 Linux 上也编。

#include <cstdint>
#include <functional>
#include <optional>
#include <string>
#include <vector>

#include "wowtype_core.h"

namespace wowtype {

/// 语音键：设置里的 VoiceKey 用 fcitx5 的键名（Alt_R、Control_R、F8……，和 Linux、macOS 一样），
/// 这里翻成 Windows 的虚拟键码。左右分开的修饰键看 right（右 Alt、右 Ctrl 带扩展键位，Win 键左右键码不同）。
struct VoiceKey {
    uint32_t virtualKey = 0; // 通用的键码：VK_MENU、VK_CONTROL、VK_LWIN、VK_F8……
    bool right = false;      // 要右边那个（Alt_R、Control_R）
    bool modifier = false;
    std::string name;

    /// 键名 → 键；留空、none、认不出返回 nullopt（语音输入就不用热键）。
    static std::optional<VoiceKey> parse(const std::string &name);
    /// 这个键是不是语音键。virtualKey 是消息里的键码（Alt 是 VK_MENU，Win 是 VK_LWIN / VK_RWIN），
    /// right 是扩展键位（右 Alt、右 Ctrl）或者 VK_R* 键码。
    bool matches(uint32_t virtualKey, bool right) const;
    bool operator==(const VoiceKey &o) const { return virtualKey == o.virtualKey && right == o.right; }
    bool operator!=(const VoiceKey &o) const { return !(*this == o); }
};

/// 正在听时音量画成一排竖条（和 Linux 插件、macOS 的 voiceWave 一样，不足 6 个时左边补最低的）。
std::string voiceWave(const std::vector<int> &levels);

/// 语音要在哪个输入框里显示、上屏（文本服务实现）。
class VoiceHost {
public:
    virtual ~VoiceHost() = default;
    /// status 是候选框里的提示（nullopt 收起），partial 是编辑区里识别到一半的字
    virtual void voiceShow(const std::optional<std::string> &status, const std::string &partial) = 0;
    virtual void voiceCommit(const std::string &text) = 0;
};

class VoiceInput {
public:
    static constexpr uint32_t kPollMs = 50;

    VoiceInput() = default;
    ~VoiceInput();
    VoiceInput(const VoiceInput &) = delete;
    VoiceInput &operator=(const VoiceInput &) = delete;

    /// 外壳开始每 kPollMs 毫秒调一次 poll（已经在调就不用管）。
    std::function<void()> onStartPolling;
    /// 识别结果要不要也放进剪贴板（VoiceCopy）由外壳做：密码框里不复制。
    std::function<void(const std::string &)> copyToClipboard;
    /// 模型在不在（没下载时不碰麦克风，只提示）。默认看 wtlocal 里有没有模型和词表。
    std::function<bool(const std::string &dir)> modelReady;
    /// 单调时钟（毫秒），测试里换成假的。
    std::function<uint64_t()> clock;

    /// 照设置配语音键；第一次要用时才建核心的语音对象（不碰麦克风、不读模型，按键时才读）。
    /// silenceMs 是点按说话时没声音多久自动结束（VoiceSilenceTimeout，毫秒），0 不自动结束。
    void configure(bool enabled, const std::string &keyName, bool copyResult, const std::string &modelDir,
                   int silenceMs = 1500);
    const std::optional<VoiceKey> &key() const { return key_; }
    bool active() const { return showing_; }

    /// 语音键按下（down）/ 松开。返回 true 表示吃掉这个键。repeat 是按住不放的自动重复。
    bool press(bool down, bool repeat, bool composing, VoiceHost *host);
    /// 别的键按下了：正在听时 Esc 取消（吃掉）、别的键取消这次语音（键照常处理，返回 false）。
    bool otherKey(bool escape, VoiceHost *host);
    /// 这个输入框没了（切走、失去焦点）：这次说的话不要了
    void cancel(VoiceHost *host);
    void cancel();
    /// 取一次状态，画出来；返回 false 表示不用再取了。
    bool poll();

private:
    void finish();
    uint64_t now() const;
    bool ready() const;

    WowtypeVoice *handle_ = nullptr;
    std::optional<VoiceKey> key_;
    bool copyResult_ = false;
    std::string modelDir_;
    bool pendingHint_ = false;
    bool keyDown_ = false;
    VoiceHost *host_ = nullptr;
    bool showing_ = false;
    std::vector<int> levels_;
    int frame_ = 0;
    uint64_t holdUntil_ = 0;
};

} // namespace wowtype
