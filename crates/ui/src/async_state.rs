//! 异步资源加载状态包：[`AsyncState<T>`] = `loading` + `load_error` + `data` 三字段。
//!
//! 收敛媒体查看器（image_viewer / video / pdf）各自重复声明的「loading/error/data」
//! 字段组与状态转移样板（`{loading=false; load_error=None; …; cx.notify()}`，Pattern 2.1）。
//! 字段名与既有语义逐字一致（`loading` / `load_error` / `data` + `is_loaded()`），
//! 使消费方（render 分支与测试断言）零漂移迁移；不引入 gpui 依赖、可裸用。

/// 异步资源加载状态（`loading` + `load_error` + `data`）。
///
/// 转移 helper（`set_loading` / `set_error` / `set_ready`）把「改字段 + 清配对字段」
/// 的样板收敛为一处；`is_loaded()` 语义与既有实现一致（`!loading && data.is_some()`）。
/// 语义约定（与媒体查看器站点现状一致）：`set_loading`/`set_error` 均**保留旧 `data`**
/// （重载期间继续显示旧内容、错误覆盖层不丢旧图）；`set_ready` 清 loading+error 并装载新 data。
#[derive(Clone)]
pub struct AsyncState<T> {
    /// 后台加载/解码进行中。
    pub loading: bool,
    /// 加载失败文案（`None` = 无错误）。
    pub load_error: Option<String>,
    /// 加载/解码结果（`None` = 尚未就绪）。
    pub data: Option<T>,
}

impl<T> Default for AsyncState<T> {
    fn default() -> Self {
        Self { loading: false, load_error: None, data: None }
    }
}

impl<T> AsyncState<T> {
    /// 初始（未加载）。
    pub fn idle() -> Self {
        Self::default()
    }

    /// 立即处于加载中。
    pub fn loading() -> Self {
        Self { loading: true, load_error: None, data: None }
    }

    /// 是否已加载完成（`!loading && data` 就绪）。
    pub fn is_loaded(&self) -> bool {
        !self.loading && self.data.is_some()
    }

    /// 是否加载/解码进行中。
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// 加载失败文案。
    pub fn error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// 已就绪数据。
    pub fn data(&self) -> Option<&T> {
        self.data.as_ref()
    }

    /// 进入加载中（清错误；保留旧 `data`——重载期间继续显示旧内容/旧图，与站点现状一致）。
    pub fn set_loading(&mut self) {
        self.loading = true;
        self.load_error = None;
    }

    /// 进入失败态（清加载中标记；保留旧 `data`——错误覆盖层显示在保留内容之上，不丢旧图）。
    pub fn set_error(&mut self, err: impl Into<String>) {
        self.loading = false;
        self.load_error = Some(err.into());
    }

    /// 进入就绪态（清除加载中标记与错误）。
    pub fn set_ready(&mut self, data: T) {
        self.loading = false;
        self.load_error = None;
        self.data = Some(data);
    }
}
