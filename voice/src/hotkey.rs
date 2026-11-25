//! 语音触发状态机：长按说话 / 点按说话。
//!
//! 语义（对齐主流输入法习惯）：
//! - 按下即开始录音
//! - 松开时按住时长 < hold_threshold(默认 300ms) => 判定为「点按」，
//!   继续录音，直到再次点按或静音超时（silence_timeout，默认 1.5 秒；None 表示不因静音结束）
//! - 松开时按住时长 >= hold_threshold => 判定为「长按」，立即结束
//! - 同一个键可以同时充当长按键和点按键

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Hold,
    Tap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// 开始录音
    Start,
    /// 继续录音（点按已识别，等待静音或再次点按）
    Continue,
    /// 结束录音
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TriggerConfig {
    pub hold_threshold: Duration,
    /// 点按模式下没声音多久自动结束；None 表示不因静音结束，只有再点一下（或外壳取消、
    /// 到了录音时长上限）才结束。长按模式不看它。
    pub silence_timeout: Option<Duration>,
}

/// 默认的静音超时：1.5 秒。
pub const DEFAULT_SILENCE_MS: u64 = 1500;

impl TriggerConfig {
    /// 设置里的毫秒数换成静音超时：0 是不因静音结束。
    pub fn silence_from_ms(ms: u64) -> Option<Duration> {
        (ms > 0).then(|| Duration::from_millis(ms))
    }
}

impl Default for TriggerConfig {
    fn default() -> Self {
        Self {
            hold_threshold: Duration::from_millis(300),
            silence_timeout: Self::silence_from_ms(DEFAULT_SILENCE_MS),
        }
    }
}

#[derive(Debug, Default)]
pub struct VoiceTrigger {
    cfg: TriggerConfig,
    recording: bool,
    mode: Mode,
    pressed_at: Option<Instant>,
    last_voice: Option<Instant>,
}

impl VoiceTrigger {
    pub fn new(cfg: TriggerConfig) -> Self {
        Self {
            cfg,
            recording: false,
            mode: Mode::Hold,
            pressed_at: None,
            last_voice: None,
        }
    }

    pub fn is_recording(&self) -> bool {
        self.recording
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// 改静音超时（设置改了）；正在点按录音时也马上按新的算，从最近一次有声音算起。
    pub fn set_silence_timeout(&mut self, timeout: Option<Duration>) {
        self.cfg.silence_timeout = timeout;
    }

    pub fn config(&self) -> TriggerConfig {
        self.cfg
    }

    /// 键按下
    pub fn on_down(&mut self, now: Instant) -> Vec<Action> {
        if self.recording {
            // 点按模式下再次按下 = 提前结束
            if self.mode == Mode::Tap {
                self.reset();
                return vec![Action::Stop];
            }
            return Vec::new();
        }
        self.recording = true;
        self.mode = Mode::Hold;
        self.pressed_at = Some(now);
        self.last_voice = Some(now);
        vec![Action::Start]
    }

    /// 键松开
    pub fn on_up(&mut self, now: Instant) -> Vec<Action> {
        let Some(pressed) = self.pressed_at.take() else {
            return Vec::new();
        };
        if !self.recording {
            return Vec::new();
        }
        let held = now.saturating_duration_since(pressed);
        if held < self.cfg.hold_threshold {
            self.mode = Mode::Tap;
            self.last_voice = Some(now);
            vec![Action::Continue]
        } else {
            self.reset();
            vec![Action::Stop]
        }
    }

    /// 有语音活动（VAD 判定为说话）
    pub fn note_voice(&mut self, now: Instant) {
        if self.recording {
            self.last_voice = Some(now);
        }
    }

    /// 周期调用；点按模式下静音超时则返回 Stop
    pub fn tick(&mut self, now: Instant) -> Option<Action> {
        if !self.recording || self.mode != Mode::Tap {
            return None;
        }
        let timeout = self.cfg.silence_timeout?;
        let last = self.last_voice.unwrap_or(now);
        if now.saturating_duration_since(last) >= timeout {
            self.reset();
            Some(Action::Stop)
        } else {
            None
        }
    }

    fn reset(&mut self) {
        self.recording = false;
        self.mode = Mode::Hold;
        self.pressed_at = None;
        self.last_voice = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TriggerConfig {
        TriggerConfig {
            hold_threshold: Duration::from_millis(300),
            silence_timeout: Some(Duration::from_millis(1500)),
        }
    }

    fn cfg_ms(ms: u64) -> TriggerConfig {
        TriggerConfig {
            silence_timeout: TriggerConfig::silence_from_ms(ms),
            ..cfg()
        }
    }

    #[test]
    fn hold_stops_on_release() {
        let t0 = Instant::now();
        let mut tr = VoiceTrigger::new(cfg());
        assert_eq!(tr.on_down(t0), vec![Action::Start]);
        assert!(tr.is_recording());
        assert_eq!(
            tr.on_up(t0 + Duration::from_millis(600)),
            vec![Action::Stop]
        );
        assert!(!tr.is_recording());
    }

    #[test]
    fn tap_keeps_recording() {
        let t0 = Instant::now();
        let mut tr = VoiceTrigger::new(cfg());
        assert_eq!(tr.on_down(t0), vec![Action::Start]);
        assert_eq!(
            tr.on_up(t0 + Duration::from_millis(100)),
            vec![Action::Continue]
        );
        assert!(tr.is_recording());
        assert_eq!(tr.mode(), Mode::Tap);
    }

    #[test]
    fn tap_stops_on_silence() {
        let t0 = Instant::now();
        let mut tr = VoiceTrigger::new(cfg());
        tr.on_down(t0);
        tr.on_up(t0 + Duration::from_millis(100));
        assert_eq!(tr.tick(t0 + Duration::from_millis(1000)), None);
        assert_eq!(
            tr.tick(t0 + Duration::from_millis(1700)),
            Some(Action::Stop)
        );
        assert!(!tr.is_recording());
    }

    #[test]
    fn voice_activity_extends_tap() {
        let t0 = Instant::now();
        let mut tr = VoiceTrigger::new(cfg());
        tr.on_down(t0);
        tr.on_up(t0 + Duration::from_millis(100));
        tr.note_voice(t0 + Duration::from_millis(1500));
        assert_eq!(tr.tick(t0 + Duration::from_millis(2000)), None);
        assert_eq!(
            tr.tick(t0 + Duration::from_millis(3100)),
            Some(Action::Stop)
        );
    }

    #[test]
    fn second_tap_stops_immediately() {
        let t0 = Instant::now();
        let mut tr = VoiceTrigger::new(cfg());
        tr.on_down(t0);
        tr.on_up(t0 + Duration::from_millis(80));
        assert_eq!(
            tr.on_down(t0 + Duration::from_millis(500)),
            vec![Action::Stop]
        );
        assert!(!tr.is_recording());
    }

    #[test]
    fn repeated_down_while_holding_is_ignored() {
        let t0 = Instant::now();
        let mut tr = VoiceTrigger::new(cfg());
        tr.on_down(t0);
        assert!(tr.on_down(t0 + Duration::from_millis(50)).is_empty());
        assert!(tr.is_recording());
    }

    #[test]
    fn up_without_down_is_noop() {
        let t0 = Instant::now();
        let mut tr = VoiceTrigger::new(cfg());
        assert!(tr.on_up(t0).is_empty());
        assert!(!tr.is_recording());
    }

    #[test]
    fn tick_when_idle_is_none() {
        let t0 = Instant::now();
        let mut tr = VoiceTrigger::new(cfg());
        assert_eq!(tr.tick(t0 + Duration::from_secs(10)), None);
    }

    #[test]
    fn hold_mode_tick_never_stops() {
        // 长按模式靠松手结束，不该被静音超时打断
        let t0 = Instant::now();
        let mut tr = VoiceTrigger::new(cfg());
        tr.on_down(t0);
        assert_eq!(tr.tick(t0 + Duration::from_secs(5)), None);
        assert!(tr.is_recording());
    }

    #[test]
    fn default_silence_is_one_and_a_half_seconds() {
        assert_eq!(
            TriggerConfig::default().silence_timeout,
            Some(Duration::from_millis(1500))
        );
        assert_eq!(TriggerConfig::silence_from_ms(0), None);
    }

    #[test]
    fn custom_silence_timeout() {
        // 设成 5 秒：停顿 3 秒还在听，5 秒没声音才结束
        let t0 = Instant::now();
        let mut tr = VoiceTrigger::new(cfg_ms(5000));
        tr.on_down(t0);
        tr.on_up(t0 + Duration::from_millis(100));
        assert_eq!(tr.tick(t0 + Duration::from_millis(1700)), None);
        assert_eq!(tr.tick(t0 + Duration::from_millis(3100)), None);
        tr.note_voice(t0 + Duration::from_millis(3200));
        assert_eq!(tr.tick(t0 + Duration::from_millis(8100)), None);
        assert!(tr.is_recording());
        assert_eq!(
            tr.tick(t0 + Duration::from_millis(8200)),
            Some(Action::Stop)
        );
        assert!(!tr.is_recording());
    }

    #[test]
    fn zero_never_stops_on_silence() {
        // 0：不因静音结束，只有再点一下才结束
        let t0 = Instant::now();
        let mut tr = VoiceTrigger::new(cfg_ms(0));
        tr.on_down(t0);
        assert_eq!(
            tr.on_up(t0 + Duration::from_millis(100)),
            vec![Action::Continue]
        );
        for secs in [2, 10, 60, 600] {
            assert_eq!(tr.tick(t0 + Duration::from_secs(secs)), None);
        }
        assert!(tr.is_recording());
        assert_eq!(tr.mode(), Mode::Tap);
        assert_eq!(
            tr.on_down(t0 + Duration::from_secs(601)),
            vec![Action::Stop]
        );
        assert!(!tr.is_recording());
    }

    #[test]
    fn hold_mode_ignores_silence_timeout() {
        // 长按不看静音超时：短的、长的、不自动结束都一样，松手才结束
        for ms in [500, 1500, 30_000, 0] {
            let t0 = Instant::now();
            let mut tr = VoiceTrigger::new(cfg_ms(ms));
            tr.on_down(t0);
            assert_eq!(tr.tick(t0 + Duration::from_secs(40)), None, "{ms}");
            assert!(tr.is_recording());
            assert_eq!(tr.mode(), Mode::Hold);
            assert_eq!(
                tr.on_up(t0 + Duration::from_secs(41)),
                vec![Action::Stop],
                "{ms}"
            );
        }
    }

    #[test]
    fn changing_timeout_applies_to_running_tap() {
        let t0 = Instant::now();
        let mut tr = VoiceTrigger::new(cfg());
        tr.on_down(t0);
        tr.on_up(t0 + Duration::from_millis(100));
        tr.set_silence_timeout(None);
        assert_eq!(tr.tick(t0 + Duration::from_secs(5)), None);
        tr.set_silence_timeout(TriggerConfig::silence_from_ms(3000));
        assert_eq!(tr.config().silence_timeout, Some(Duration::from_secs(3)));
        assert_eq!(tr.tick(t0 + Duration::from_secs(5)), Some(Action::Stop));
    }
}
