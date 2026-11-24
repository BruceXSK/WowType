// 设置窗口「输入」页的「跨设备复制粘贴」一组用的状态：配对、取消配对、中转服务的访问口令和测试。
// 配对在这个进程（设置窗口）里做，结果写进配对文件；输入法那个进程盯着这个文件，自己跟上。
// 界面在 SettingsView.swift 的 SyncGroup。说明见 docs/SYNC.md。

import AppKit
import SwiftUI

final class SyncSettings: ObservableObject {
    /// 配对文件里记着的：配没配对、和谁配的
    @Published private(set) var paired = false
    @Published private(set) var devices: [String] = []
    /// 正在显示的配对码、「正在找另一台设备…」；没有是 nil
    @Published private(set) var note: String?
    /// 正在显示配对码（按钮变成「取消」）
    @Published private(set) var hosting = false
    /// 正在加入、正在测试中转服务
    @Published private(set) var busy = false
    @Published private(set) var tokenSet = false
    @Published private(set) var pingResult: String?

    private var pairing: SyncPairing?
    private var timer: Timer?
    private let statePath: String
    private let keyPath: String
    private let hostName: String

    init(state: AppState = .shared) {
        statePath = state.syncStatePath
        keyPath = state.syncRelayKeyPath
        hostName = state.syncHostName
        refresh()
    }

    deinit {
        timer?.invalidate()
    }

    /// ClipboardSyncDeviceName 留空时用的名字（输入框的占位文字）
    var defaultName: String { hostName }

    func refresh() {
        let state = SyncFiles.devices(stateFile: statePath)
        paired = state.paired
        devices = state.names
        tokenSet = !TranslateKeyFile.read(keyPath).isEmpty
    }

    /// 「配对」那一行的说明
    var summary: String {
        if let note { return note }
        if paired {
            return devices.isEmpty ? "已经配对" : "已经和 " + devices.joined(separator: "、") + " 配对"
        }
        return "还没有配对：在一台设备上显示配对码，在另一台上输入"
    }

    private func settings(_ config: WowtypeConfig) -> SyncPairSettings {
        SyncPairSettings(stateFile: statePath,
                         deviceName: config.clipboardSyncDeviceName.isEmpty ? hostName : config.clipboardSyncDeviceName,
                         lan: config.clipboardSyncLan, relayUrl: config.clipboardSyncRelayUrl, relayKeyFile: keyPath)
    }

    /// 「显示配对码」：等另一台设备输入。再点一次（「取消」）就停。
    func toggleCode(config: WowtypeConfig, toast: @escaping (String) -> Void) {
        if hosting {
            finish()
            return
        }
        guard pairing == nil, !busy else { return }
        hosting = true
        note = "正在准备配对码…"
        watch(SyncPairing(host: settings(config)), toast: toast)
    }

    /// 输入了另一台设备上显示的配对码。配上了返回 true 的回调里清空输入框。
    func join(code: String, config: WowtypeConfig, toast: @escaping (String) -> Void, done: @escaping () -> Void) {
        guard pairing == nil, !busy else { return }
        guard let code = SyncFiles.normalize(code: code) else {
            toast("配对码是 6 位数字")
            return
        }
        busy = true
        note = "正在找另一台设备…"
        watch(SyncPairing(join: settings(config), code: code), toast: toast, done: done)
    }

    private func watch(_ pairing: SyncPairing, toast: @escaping (String) -> Void, done: (() -> Void)? = nil) {
        self.pairing = pairing
        let timer = Timer(timeInterval: 0.2, repeats: true) { [weak self] _ in
            guard let self, let pairing = self.pairing else { return }
            switch pairing.state {
            case .starting:
                break
            case .waiting(let code):
                self.note = "配对码 \(SyncFiles.spaced(code))：在另一台设备的 WowType 设置里输入，5 分钟内有效"
            case .done(let device):
                self.finish()
                toast("已经和 \(device) 配对")
                done?()
            case .failed(let reason):
                self.finish()
                toast(reason)
            }
        }
        RunLoop.main.add(timer, forMode: .common)
        self.timer = timer
    }

    /// 收尾：放掉这次配对（还在等的话就是取消），重新读配对文件。
    private func finish() {
        timer?.invalidate()
        timer = nil
        pairing = nil
        hosting = false
        busy = false
        note = nil
        refresh()
    }

    /// 取消配对：先问一句。只动这台设备。
    func unpair(toast: @escaping (String) -> Void) {
        let alert = NSAlert()
        alert.messageText = "取消配对？"
        alert.informativeText = "这台设备不再收到别的设备复制的内容，也不再把这里复制的发出去。"
            + "别的设备之间不受影响；以后想恢复，重新配一次对就行。"
        alert.addButton(withTitle: "取消配对")
        alert.addButton(withTitle: "不取消")
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        toast(SyncFiles.unpair(stateFile: statePath) ? "已取消配对" : "配对文件改不了")
        refresh()
    }

    /// 存中转服务的访问口令（空串删掉）。单独一个只有自己能读的文件，不进 wowtype.conf。
    func setToken(_ token: String, toast: (String) -> Void) {
        if TranslateKeyFile.write(token, to: keyPath) {
            refresh()
            toast(tokenSet ? "访问口令已保存" : "访问口令已删除")
        } else {
            toast("访问口令没写进去：\(keyPath)")
        }
    }

    /// 测试中转服务：要连网，放后台线程。
    func ping(config: WowtypeConfig) {
        guard !busy else { return }
        let relay = config.clipboardSyncRelayUrl
        guard !relay.isEmpty else {
            pingResult = "先填中转服务的地址"
            return
        }
        busy = true
        pingResult = "正在连接…"
        let keyPath = self.keyPath
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let problem = SyncFiles.ping(relayUrl: relay, relayKeyFile: keyPath)
            DispatchQueue.main.async {
                guard let self else { return }
                self.busy = false
                self.pingResult = problem ?? "连得上，访问口令也对"
            }
        }
    }
}
