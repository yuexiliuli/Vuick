//! 浮层「浮现 → 排队自隐」时序收敛：image 锁定信息条 / video 纯视频控制条两处原本
//! 逐字同构的「代序号（防旧定时器误清新浮现）+ 延时自隐 + hover 抑制」收敛为一个小型
//! 状态机（[`AutoHide`]）+ 统一的浮现/排队入口（[`AutoHide::show_temporarily`]）。
//!
//! 职责边界（收敛目标）：
//! - 只收「何时该隐藏」的**纯判定**与「代序 / 悬停 / 可见」状态，不吸附具体查看器；
//! - 是否处于浮动模式（image 的锁定；video 恒真）由调用方以 `mode` 条件传入，
//!   定时器到期时**复读**（窗口期内解锁可中止排队中的自隐）；
//! - 定时器经 gpui `Context` 由 [`AutoHide::show_temporarily`] 统一发起，调用方
//!   只写一行「浮现 + 排队自隐」；hover 进出由 `hover_in` / `hover_out` 改状态。
//!
//! **不**支持：多浮层共用一个 `AutoHide`（两家查看器各自持有实例，互不串扰）；
//! 非本浮层的自隐（video `status` 状态浮层）仍留在各自查看器。

use gpui::Context;
use std::time::Duration;

/// 浮层自隐状态机：`visible` 可见位 + `seq` 代序号 + `hovering` 悬停抑制 + `delay` 延时。
pub struct AutoHide {
    /// 当前是否应显示（调用方仅在浮动模式消费；普通模式常驻与否由调用方渲染判断）。
    visible: bool,
    /// 定时器代序号：每次浮现 / hover 进入递增；定时器到期与当前代序不符即作废
    /// （旧定时器不能误清新浮现，反复快速切图的「定时器风暴」因此收敛）。
    seq: u64,
    /// 是否悬停在该浮层的热区上：悬停期间即使代序匹配也不隐藏。
    hovering: bool,
    /// 自隐延时（可参数化；默认 1 秒）。
    delay: Duration,
}

impl AutoHide {
    /// 默认 1 秒自隐。
    pub fn new() -> Self {
        Self::with_delay(Duration::from_secs(1))
    }

    /// 指定自隐延时。
    pub fn with_delay(delay: Duration) -> Self {
        Self {
            visible: false,
            seq: 0,
            hovering: false,
            delay,
        }
    }

    /// 浮现：置可见、递增代序并返回本次代序（配对自隐定时器时交给定时器核对用）。
    pub fn show(&mut self) -> u64 {
        self.visible = true;
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    /// hover 进入浮层热区：置可见 + 作废排队中的自隐（代序递增），悬停期间不隐藏。
    pub fn hover_in(&mut self) {
        self.hovering = true;
        self.show();
    }

    /// hover 移开：解除悬停抑制（是否重排自隐由调用方决定，通常随后调
    /// [`AutoHide::show_temporarily`]）。
    pub fn hover_out(&mut self) {
        self.hovering = false;
    }

    /// 纯函数：定时器到期后是否该隐藏——当前代序匹配、非悬停、且模式条件 `active`
    /// 成立（image 的锁定 / video 恒真；未锁定或已解锁即不隐藏）。
    pub fn should_hide(&self, seq: u64, active: bool) -> bool {
        self.seq == seq && !self.hovering && active
    }

    /// 统一入口：浮现 + 排队「延时后自隐」。
    ///
    /// - `mode_active`：模式条件（如 image 的 `|v| v.locked`），定时器到期时**复读**，
    ///   未锁定 / 已解锁即中止排队中的自隐；video 恒 `|_| true`；
    /// - `rebind`：定时器内在 `&mut T` 上取回本浮层状态的取柄（调用方一行闭包）。
    pub fn show_temporarily<T: 'static>(
        &mut self,
        mode_active: fn(&T) -> bool,
        rebind: fn(&mut T) -> &mut AutoHide,
        cx: &mut Context<T>,
    ) {
        let seq = self.show();
        cx.notify();
        let delay = self.delay;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                let active = mode_active(this);
                let auto = rebind(this);
                if auto.should_hide(seq, active) {
                    auto.visible = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn hovering(&self) -> bool {
        self.hovering
    }

    /// 当前代序号（测试断言 / hover 抑制交互用）。
    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn delay(&self) -> Duration {
        self.delay
    }
}

impl Default for AutoHide {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, Context, TestAppContext};

    struct Floater {
        auto: AutoHide,
        active: bool,
    }

    impl Floater {
        fn show(&mut self, cx: &mut Context<Self>) {
            self.auto.show_temporarily(|_| true, |f| &mut f.auto, cx);
        }

        fn show_if_active(&mut self, cx: &mut Context<Self>) {
            self.auto.show_temporarily(|f| f.active, |f| &mut f.auto, cx);
        }

        fn set_hover(&mut self, hovering: bool, cx: &mut Context<Self>) {
            if hovering {
                self.auto.hover_in();
            } else {
                self.auto.hover_out();
            }
            cx.notify();
        }
    }

    /// 初次浮现即记代序；代序递增后旧定时器到期不得再隐藏（反复切图的定时器风暴收敛）。
    #[test]
    fn seq_staleness_prevents_old_timer_from_hiding_fresh_show() {
        let mut a = AutoHide::new();
        assert!(!a.visible(), "初始隐藏");
        let first = a.show();
        assert!(a.visible());
        let second = a.show();
        assert_ne!(first, second, "每次浮现代序递增");
        assert!(!a.should_hide(first, true), "旧代序定时器不得隐藏新浮现");
        assert!(a.should_hide(second, true), "当前代序 + 非悬停 + 模式活跃 → 允许隐藏");
    }

    /// hover 进入：可见 + 作废排队自隐；悬停期间即使代序匹配也不隐藏；移开重排后可隐藏。
    #[test]
    fn hover_suppresses_hide_and_out_restores() {
        let mut a = AutoHide::new();
        let s1 = a.show();
        a.hover_in();
        assert!(a.visible() && a.hovering());
        assert!(!a.should_hide(s1, true), "悬停期间即使代序匹配也不隐藏");
        // 移开后按真实流程重排：新浮现带新代序，此刻可隐藏；旧代序永远作废
        a.hover_out();
        assert!(!a.hovering());
        let s2 = a.show();
        assert!(a.should_hide(s2, true), "移开并重排后恢复可隐藏");
        assert!(!a.should_hide(s1, true), "被作废的旧代序仍不该隐藏");
    }

    /// 模式条件（未锁定 / 未浮动）：定时器到期不隐藏 = 常驻；切入模式后恢复可隐藏。
    #[test]
    fn mode_inactive_keeps_overlay_persistent() {
        let mut a = AutoHide::new();
        let seq = a.show();
        assert!(!a.should_hide(seq, false), "未锁定（模式不活跃）不隐藏");
        assert!(a.should_hide(seq, true), "锁定后恢复可隐藏");
    }

    /// 延时可参数化（调用方 new / with_delay 决定）。
    #[test]
    fn delay_is_parameterizable() {
        assert_eq!(AutoHide::new().delay(), Duration::from_secs(1));
        assert_eq!(
            AutoHide::with_delay(Duration::from_millis(50)).delay(),
            Duration::from_millis(50)
        );
    }

    /// 多浮层各自独立：一个浮层的 hover / 代序不影响另一个。
    #[test]
    fn multiple_overlays_do_not_crosstalk() {
        let mut info = AutoHide::new();
        let mut controls = AutoHide::new();
        let info_seq = info.show();
        controls.show();
        controls.hover_in();
        assert!(info.should_hide(info_seq, true), "info 不受 controls 悬停影响");
        assert!(!controls.should_hide(controls.seq(), true), "controls 自己被悬停抑制");
        assert!(info.visible() && controls.visible());
    }

    /// `show_temporarily` 全链路（真定时器 + 代序 + hover + 模式 + 双实例独立）：
    /// 浮现 → 1s 后自隐；窗内再次浮现重置 1s（旧定时器不提前隐藏）；悬停抑制；
    /// 模式不活跃常驻；两个浮层互不串扰。
    #[gpui::test]
    async fn show_temporarily_auto_hides_with_reset_hover_and_mode(cx: &mut TestAppContext) {
        let ent = cx.update(|app| {
            app.new(|cx| Floater {
                auto: AutoHide::with_delay(Duration::from_millis(1000)),
                active: true,
            })
        });
        assert!(!cx.update(|app| ent.read(app).auto.visible()));
        ent.update(cx, |f, cx| f.show(cx));
        assert!(cx.update(|app| ent.read(app).auto.visible()), "浮现立即可见");
        cx.executor().advance_clock(Duration::from_millis(900));
        cx.run_until_parked();
        assert!(cx.update(|app| ent.read(app).auto.visible()), "900ms 未到 1s");
        // 窗内再次浮现：旧定时器被代序作废，不得在 200ms 后提前隐藏
        ent.update(cx, |f, cx| f.show(cx));
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        assert!(
            cx.update(|app| ent.read(app).auto.visible()),
            "再次浮现后旧定时器不得提前隐藏"
        );
        cx.executor().advance_clock(Duration::from_millis(800));
        cx.run_until_parked();
        assert!(!cx.update(|app| ent.read(app).auto.visible()), "新 1s 窗口结束自隐");
        // 悬停抑制：浮现后悬停，过 2s 也不隐藏
        ent.update(cx, |f, cx| f.show(cx));
        ent.update(cx, |f, cx| f.set_hover(true, cx));
        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        assert!(cx.update(|app| ent.read(app).auto.visible()), "悬停期间不隐藏");
        // 移开 + 重排 → 1s 后自隐
        ent.update(cx, |f, cx| f.set_hover(false, cx));
        ent.update(cx, |f, cx| f.show(cx));
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert!(!cx.update(|app| ent.read(app).auto.visible()), "移开并重排后 1s 自隐");
        // 模式不活跃（未锁定）：显示后过 2s 常驻不隐藏
        let inert = cx.update(|app| {
            app.new(|cx| Floater {
                auto: AutoHide::new(),
                active: false,
            })
        });
        inert.update(cx, |f, cx| f.show_if_active(cx));
        assert!(cx.update(|app| inert.read(app).auto.visible()));
        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        assert!(
            cx.update(|app| inert.read(app).auto.visible()),
            "模式不活跃时不隐藏（常驻）"
        );
        // 双实例独立：ent 已隐藏，inert 仍常驻 —— 互不串扰
        assert!(!cx.update(|app| ent.read(app).auto.visible()));
        assert!(cx.update(|app| inert.read(app).auto.visible()));
    }
}
