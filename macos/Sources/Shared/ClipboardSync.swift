// 跨设备复制粘贴（docs/SYNC.md）：核心的 wowtype_sync_* 包一层。逻辑都在核心里（配对、加密、局域网、中转服务），
// 这里不依赖 AppKit，测试在 Linux 上也能跑。读写系统剪贴板在 App 层（AppState）。

import CWowtype
import Foundation

/// 输入法里跑的那一份同步服务。核心没带同步（不带 sync 特性编的）时什么都不做。
public final class ClipboardSync {
    private var handle: OpaquePointer?

    public init() {}

    deinit {
        if let handle { wowtype_sync_free(handle) }
    }

    /// 照设置起停。设置没变时核心什么都不做，随便调。
    /// hostName 是 ClipboardSyncDeviceName 留空时用的名字。
    public func configure(_ config: WowtypeConfig, stateFile: String, relayKeyFile: String, hostName: String) {
        if handle == nil {
            // 没打开过就不建（不起线程）
            guard config.clipboardSync else { return }
            handle = wowtype_sync_new()
        }
        guard let handle else { return }
        let name = config.clipboardSyncDeviceName.isEmpty ? hostName : config.clipboardSyncDeviceName
        stateFile.withCString { state in
            relayKeyFile.withCString { key in
                config.clipboardSyncRelayUrl.withCString { relay in
                    name.withCString { device in
                        var raw = WowtypeSyncConfig()
                        raw.size = UInt32(MemoryLayout<WowtypeSyncConfig>.size)
                        raw.enabled = config.clipboardSync ? 1 : 0
                        raw.lan = config.clipboardSyncLan ? 1 : 0
                        raw.relay_url = relay
                        raw.relay_key_file = key
                        raw.state_file = state
                        raw.device_name = device
                        wowtype_sync_configure(handle, &raw)
                    }
                }
            }
        }
    }

    /// 本机剪贴板现在是这段文字。和上次报的不一样、又不是别的设备刚传来的就发出去；返回发了没有。
    /// configure 以后第一次报的只当基准，所以每隔一会儿都要报一次（没变就报同一段），空的也报。
    @discardableResult
    public func observe(_ text: String) -> Bool {
        guard let handle else { return false }
        return wowtype_sync_observe(handle, text) != 0
    }

    /// 有人在用输入法：中转服务那边保持连接。
    public func touch() {
        if let handle { wowtype_sync_touch(handle) }
    }

    /// 取一条别的设备复制过来的（核心同时记进了剪贴板历史）；没有返回 nil。
    public func poll() -> (text: String, from: String)? {
        guard let handle, let text = wowtype_sync_poll(handle) else { return nil }
        return (String(cString: text), String(cString: wowtype_sync_from(handle)))
    }

    /// 现在的状态（核心给的 JSON 解出来）；没建过是 nil。
    public var status: [String: Any]? {
        guard let handle, let text = wowtype_sync_status(handle) else { return nil }
        return SyncFiles.object(String(cString: text))
    }
}

/// 配对用的设置：和输入法那边用同一个配对文件、同一个中转服务。
public struct SyncPairSettings {
    public var stateFile: String
    public var deviceName: String
    public var lan: Bool
    public var relayUrl: String
    public var relayKeyFile: String

    public init(stateFile: String, deviceName: String, lan: Bool, relayUrl: String, relayKeyFile: String) {
        self.stateFile = stateFile
        self.deviceName = deviceName
        self.lan = lan
        self.relayUrl = relayUrl
        self.relayKeyFile = relayKeyFile
    }

    fileprivate func withRaw<T>(_ body: (UnsafePointer<WowtypeSyncPairConfig>) -> T) -> T {
        stateFile.withCString { state in
            deviceName.withCString { name in
                relayUrl.withCString { relay in
                    relayKeyFile.withCString { key in
                        var raw = WowtypeSyncPairConfig()
                        raw.size = UInt32(MemoryLayout<WowtypeSyncPairConfig>.size)
                        raw.state_file = state
                        raw.device_name = name
                        raw.lan = lan ? 1 : 0
                        raw.relay_url = relay
                        raw.relay_key_file = key
                        return body(&raw)
                    }
                }
            }
        }
    }
}

/// 一次配对（主持或者加入）。在核心自己的线程里走，界面定时看 state。放掉就是取消。
public final class SyncPairing {
    public enum State: Equatable {
        /// 还在准备（主持）、正在找另一台（加入）
        case starting
        /// 主持：等另一台输入这个配对码
        case waiting(code: String)
        /// 配好了，另一台叫这个名字
        case done(device: String)
        case failed(reason: String)
    }

    private let handle: OpaquePointer?

    /// 主持：生成配对码，等另一台设备输入。
    public init(host settings: SyncPairSettings) {
        handle = settings.withRaw { wowtype_sync_pair_host($0) }
    }

    /// 加入：拿另一台设备上显示的配对码去配。
    public init(join settings: SyncPairSettings, code: String) {
        handle = settings.withRaw { raw in code.withCString { wowtype_sync_pair_join(raw, $0) } }
    }

    deinit {
        if let handle { wowtype_sync_pairing_free(handle) }
    }

    public var state: State {
        guard let handle else { return .failed(reason: "这个版本没带跨设备复制粘贴") }
        let text = String(cString: wowtype_sync_pairing_text(handle))
        switch wowtype_sync_pairing_state(handle) {
        case Int32(WOWTYPE_SYNC_PAIR_STARTING): return .starting
        case Int32(WOWTYPE_SYNC_PAIR_WAITING): return .waiting(code: text)
        case Int32(WOWTYPE_SYNC_PAIR_DONE): return .done(device: text)
        default: return .failed(reason: text)
        }
    }
}

/// 配对文件、中转服务：设置窗口里不经输入法直接问的那几样。
public enum SyncFiles {
    static func object(_ json: String) -> [String: Any]? {
        (try? JSONSerialization.jsonObject(with: Data(json.utf8))) as? [String: Any]
    }

    /// 配没配对、一起配过对的设备的名字（不读密钥）。
    public static func devices(stateFile: String) -> (paired: Bool, names: [String]) {
        guard let text = wowtype_sync_paired(stateFile), let state = object(String(cString: text)) else {
            return (false, [])
        }
        return (state["paired"] as? Bool ?? false, state["devices"] as? [String] ?? [])
    }

    /// 这台设备取消配对。
    @discardableResult
    public static func unpair(stateFile: String) -> Bool {
        wowtype_sync_unpair(stateFile) == 0
    }

    /// 测试中转服务：会等到有结果（要连网），放后台线程里调。能用返回 nil，不能用返回原因。
    public static func ping(relayUrl: String, relayKeyFile: String) -> String? {
        if wowtype_sync_ping(relayUrl, relayKeyFile) == 0 { return nil }
        return String(cString: wowtype_last_error())
    }

    /// 配对码显示成「123 456」。
    public static func spaced(_ code: String) -> String {
        guard code.count == 6 else { return code }
        return String(code.prefix(3)) + " " + String(code.suffix(3))
    }

    /// 输入的配对码：去掉空格、短横；不是 6 位数字返回 nil。
    public static func normalize(code: String) -> String? {
        let digits = code.filter { !$0.isWhitespace && $0 != "-" }
        guard digits.count == 6, digits.allSatisfy({ $0.isASCII && $0.isNumber }) else { return nil }
        return digits
    }
}
