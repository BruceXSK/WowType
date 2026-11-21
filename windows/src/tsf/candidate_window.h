#pragma once

// 候选框：自己画的无边框弹出窗口（分层窗口，不抢焦点），Direct2D + DirectWrite 画字（彩色 emoji）。
// 和 macOS 的 CandidatePanel 对应：横排一行、竖排一列、展开面板 / 表情面板是 6 列的网格，
// 互译时上面多原文、下面多一行状态；光标处的候选高亮，有上一页 / 下一页时画两个小箭头。
// 鼠标点候选、点箭头、滚轮交给文本服务（onSelect、onPage）；右键点学过的词出菜单「忘掉这个词」
// （canForget、onForget，#229）。也拿来挂定时器（互译轮询、模式提示）。

#include <windows.h>

#include <d2d1.h>
#include <dwrite.h>
#include <wrl/client.h>

#include <functional>
#include <string>
#include <vector>

#include "../common/composer.h"

namespace wowtype::tsf {

class CandidateWindow {
public:
    CandidateWindow() = default;
    ~CandidateWindow();
    CandidateWindow(const CandidateWindow &) = delete;
    CandidateWindow &operator=(const CandidateWindow &) = delete;

    std::function<void(int index)> onSelect;
    std::function<void(bool forward)> onPage;
    /// 右键点了显示的第 index 个：能不能「忘掉这个词」、点了菜单
    std::function<bool(int index)> canForget;
    std::function<void(int index)> onForget;
    std::function<void(UINT_PTR id)> onTimer;

    /// 画 page，放在 anchor（屏幕坐标里编辑区或光标的框）下面；放不下就放上面。
    void show(const CandidatePage &page, const RECT &anchor, int fontSize);
    void hide();
    bool visible() const { return hwnd_ && IsWindowVisible(hwnd_); }

    void setTimer(UINT_PTR id, UINT ms);
    void killTimer(UINT_PTR id);

private:
    struct Item {
        std::wstring label;
        std::wstring text;
        std::wstring comment;
        int index = 0;
        D2D1_RECT_F rect{};
        float labelWidth = 0;
        float textWidth = 0;
    };
    struct TextLine {
        std::wstring text;
        bool muted = false;
        float y = 0;
    };

    bool ensureWindow();
    bool ensureResources();
    void layout();
    void paint();
    float measure(const std::wstring &text, IDWriteTextFormat *format, float *height = nullptr) const;
    /// 客户区坐标（物理像素）下的候选；不在候选上是 nullptr。
    const Item *itemAt(LPARAM lParam) const;
    /// 右键点了候选：学过的词弹菜单「忘掉这个词」，选了交给 onForget。
    void showForgetMenu(LPARAM lParam);
    static LRESULT CALLBACK windowProc(HWND hwnd, UINT message, WPARAM wParam, LPARAM lParam);
    LRESULT handle(UINT message, WPARAM wParam, LPARAM lParam);

    HWND hwnd_ = nullptr;
    Microsoft::WRL::ComPtr<ID2D1Factory> d2d_;
    Microsoft::WRL::ComPtr<IDWriteFactory> dwrite_;
    Microsoft::WRL::ComPtr<ID2D1HwndRenderTarget> target_;
    Microsoft::WRL::ComPtr<IDWriteTextFormat> textFormat_;
    Microsoft::WRL::ComPtr<IDWriteTextFormat> smallFormat_;
    int formatSize_ = 0;
    float formatScale_ = 0;

    CandidatePage page_;
    int fontSize_ = 17;
    std::vector<Item> items_;
    std::vector<TextLine> lines_;
    D2D1_RECT_F prevArrow_{};
    D2D1_RECT_F nextArrow_{};
    bool hasArrows_ = false;
    float width_ = 0;
    float height_ = 0;
    float lineHeight_ = 0;
    bool dark_ = false;
};

} // namespace wowtype::tsf
