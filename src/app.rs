//! 应用主视图 `ChatApp`：AI 文档问答聊天界面。
//!
//! 布局（自上而下）：
//! - 顶部标题栏：标题 + 徽标 + 状态文字 + "清空"按钮
//! - 消息区：可滚动，展示历史轮次（用户问句 + 深度思考折叠 + Markdown 回答）
//!   以及当前流式生成中的一问一答
//! - 底部输入栏：多行输入框（Enter 发送，Shift+Enter 换行）+ 发送按钮
//!
//! 网络逻辑全部放到后台线程（[`backend::DifyClient`]），通过 `mpsc` + 轮询
//! 把流式增量推回主线程渲染。

use std::rc::Rc;
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

use gpui::{
    div, px, rgb, white, App, AppContext, Context, CursorStyle, ElementId, FocusHandle, Focusable,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window,
};

/// 输入框最大可视高度（像素）。行数超过后输入框内部滚动，不再把主窗口撑高。
/// 可在这里调节（设置）输入框高度上限。
const INPUT_MAX_HEIGHT: Pixels = px(150.);

use super::backend::DifyClient;
use super::markdown::{break_long_runs, render_markdown_with_options, selected_markdown, SelectCtx, TableMode, MarkdownTheme};
use super::scrollbar::TableScrollState;
use super::input::{self, TextArea, TextInputEvent};

/// 后台流式线程发往主线程的事件。
enum StreamEvent {
    Delta {
        thinking: String,
        answer: String,
        status: String,
    },
    Done(Result<(String, String), String>),
}

/// 一轮（一问一答）。
struct Turn {
    question: String,
    thinking: String,
    answer: String,
    thinking_open: bool,
}

/// 当前正在流式生成的一轮。
struct StreamState {
    question: String,
    thinking: String,
    answer: String,
    /// 流式中表格的横向滚动（跨增量保持横向位置与拖拽状态）
    table_scroll: TableScrollState,
    /// 流式回答的选区状态
    selection: Rc<SelectCtx>,
}

/// "复制"按钮的目标：某条回答 / 某条思考。
#[derive(Clone, Copy)]
enum CopyTarget {
    Answer(usize),
    Thinking(usize),
}

pub struct ChatApp {
    /// 问题输入框
    pub input: gpui::Entity<TextArea>,
    /// Dify 客户端（可克隆到后台线程）
    client: DifyClient,
    /// 历史轮次
    messages: Vec<Turn>,
    /// 正在流式生成的一轮（有值即表示忙碌）
    stream: Option<StreamState>,
    /// 顶部状态文字与是否"正常"
    status: SharedString,
    status_ok: bool,
    /// 消息区滚动句柄
    messages_scroll: gpui::ScrollHandle,
    /// 表格超宽的处理方式（"自动换行" / "横向滚动"）
    table_mode: TableMode,
    /// 每个历史轮次各自的表格横向滚动状态（与 `messages` 对齐）
    table_scrolls: Vec<TableScrollState>,
    /// 每个历史轮次的回答选区状态（与 `messages` 对齐）
    selections: Vec<Rc<SelectCtx>>,
    /// 回答/轮次的自增编号（保证选区 id 跨轮唯一）
    next_turn: u64,
    /// FPS 调试覆盖层（`CHAT_FPS=1` 开启）
    fps_on: bool,
    fps_last: Option<Instant>,
    fps_ema: f32,
    /// 订阅（保持有效）
    _subscription: Option<Subscription>,
    focus_handle: FocusHandle,
}

impl Focusable for ChatApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ChatApp {
    /// 创建并注册 `ChatApp` 实体（在 App 上下文中使用）。
    pub fn build(cx: &mut App) -> gpui::Entity<ChatApp> {
        cx.new(|cx| Self::new(cx))
    }

    /// 构造（测试也可复用）。
    pub fn new(cx: &mut Context<Self>) -> ChatApp {
        let focus_handle = cx.focus_handle();
        let input_focus = cx.focus_handle();
        let input = cx.new(|_cx| {
            TextArea::new(
                input_focus,
                SharedString::from("输入问题，Enter 发送，Shift+Enter 换行"),
                SharedString::from(""),
                true,
            )
            .with_submit_on_enter()
        });

        let mut app = ChatApp {
            input,
            client: DifyClient::new(),
            messages: Vec::new(),
            stream: None,
            status: SharedString::from("正在连接..."),
            status_ok: false,
            messages_scroll: gpui::ScrollHandle::new(),
            table_mode: TableMode::Wrap,
            table_scrolls: Vec::new(),
            selections: Vec::new(),
            next_turn: 0,
            fps_on: std::env::var("CHAT_FPS")
                .map(|v| v != "0" && v != "false")
                .unwrap_or(false),
            fps_last: None,
            fps_ema: 0.0,
            _subscription: None,
            focus_handle,
        };

        // 回车即发送
        let input = app.input.clone();
        let sub = cx.subscribe(&input, |this, _input, _: &TextInputEvent, cx| {
            let q = this.input.read(cx).content.to_string();
            if !q.trim().is_empty() {
                this.start_question(&q, cx);
            }
        });
        app._subscription = Some(sub);

        // 启动时后台上传默认文档
        app.kick_upload_doc(cx);

        app
    }

    /// 在后台线程上传默认文档，完成后把状态推回主线程。
    fn kick_upload_doc(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let task = cx
                .background_executor()
                .spawn(async move { client.upload_default_doc() });
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.apply_status(match result {
                    Ok(m) => m,
                    Err(e) => format!("文档准备失败: {}", e),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// 依据状态文案刷新顶栏颜色（失败/上传中→警告，否则正常）。
    fn apply_status(&mut self, msg: String) {
        let ok = !msg.contains("失败") && !msg.contains("上传");
        self.status = msg.into();
        self.status_ok = ok;
    }

    fn current_question(&self, cx: &mut Context<Self>) -> String {
        self.input.read(cx).content.to_string()
    }

    /// 发起一问（从输入框取内容）。
    fn send_from_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let q = self.current_question(cx);
        if !q.trim().is_empty() {
            self.start_question(&q, cx);
        }
        window.focus(&self.input.read(cx).focus_handle.clone());
    }

    /// 把某条历史回答的 **markdown 源码** 写入剪贴板（按钮/右键共用），并提示"已复制"。
    fn copy_turn_answer(&mut self, index: usize, window: &mut Window, cx: &mut App) {
        let Some(text) = self.messages.get(index).map(|t| t.answer.clone()) else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.status = SharedString::from("已复制（markdown）");
        self.status_ok = true;
        window.refresh();
    }

    /// 把某条历史思考的纯文本写入剪贴板。
    fn copy_turn_thinking(&mut self, index: usize, window: &mut Window, cx: &mut App) {
        let Some(text) = self.messages.get(index).map(|t| t.thinking.clone()) else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.status = SharedString::from("已复制思考");
        self.status_ok = true;
        window.refresh();
    }

    /// 清空会话。
    fn clear(&mut self, cx: &mut Context<Self>) {
        if self.stream.is_some() {
            return;
        }
        self.messages.clear();
        self.table_scrolls.clear();
        self.selections.clear();
        self.status = SharedString::from("已清空对话");
        self.status_ok = true;
        let client = self.client.clone();
        cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .spawn(async move { client.clear_chat() })
                .detach();
        })
        .detach();
        self.messages_scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 核心：后台流式调用 Dify，把增量推回界面。
    fn start_question(&mut self, question: &str, cx: &mut Context<Self>) {
        let question = question.trim().to_string();
        if question.is_empty() || self.stream.is_some() {
            return;
        }
        // 清空输入框
        self.input.update(cx, |input, cx| {
            input.content = SharedString::from("");
            input.selected_range = 0..0;
            cx.notify();
        });
        self.busy_begin(question.clone(), cx);

        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let (tx, rx) = mpsc::channel::<StreamEvent>();
            // 阻塞式网络调用放到后台线程
            cx.background_executor()
                .spawn(async move {
                    let tx_delta = tx.clone();
                    let result = client.stream_question(&question, move |t, a, s| {
                        let _ = tx_delta.send(StreamEvent::Delta {
                            thinking: t.to_string(),
                            answer: a.to_string(),
                            status: s.to_string(),
                        });
                    });
                    let _ = tx.send(StreamEvent::Done(result));
                })
                .detach();

            // 主线程轮询，把增量刷到界面上
            loop {
                loop {
                    match rx.try_recv() {
                        Ok(StreamEvent::Delta {
                            thinking,
                            answer,
                            status,
                        }) => {
                            let _ = this.update(&mut *cx, |this, cx| {
                                if let Some(st) = &mut this.stream {
                                    st.thinking = thinking;
                                    st.answer = answer;
                                }
                                this.status = status.into();
                                this.status_ok = false;
                                this.messages_scroll.scroll_to_bottom();
                                cx.notify();
                            });
                        }
                        Ok(StreamEvent::Done(result)) => {
                            let _ = this.update(&mut *cx, |this, cx| {
                                this.busy_end(result);
                                cx.notify();
                            });
                            return;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            let _ = this.update(&mut *cx, |this, cx| {
                                this.busy_end(Err("连接中断".into()));
                                cx.notify();
                            });
                            return;
                        }
                    }
                }
                cx.background_executor()
                    .timer(Duration::from_millis(10))
                    .await;
            }
        })
        .detach();
    }

    fn busy_begin(&mut self, question: String, cx: &mut Context<Self>) {
        let turn = self.next_turn;
        self.next_turn += 1;
        let selection = Rc::new(SelectCtx::new(turn, self.focus_handle.clone()));
        self.stream = Some(StreamState {
            question,
            thinking: String::new(),
            answer: String::new(),
            table_scroll: TableScrollState::new(),
            selection,
        });
        self.status = SharedString::from("正在深度思考...");
        self.status_ok = false;
        self.messages_scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 流式结束：把结果写入历史 / 显示错误。
    fn busy_end(&mut self, result: Result<(String, String), String>) {
        let Some(st) = self.stream.take() else {
            return;
        };
        let mut thinking = st.thinking.trim().to_string();
        let mut answer = st.answer.trim().to_string();
        match result {
            Ok((t, a)) => {
                if !t.is_empty() {
                    thinking = t;
                }
                if !a.is_empty() {
                    answer = a;
                }
                self.status = SharedString::from("");
                self.status_ok = true;
            }
            Err(e) => {
                self.status = SharedString::from(format!("请求失败: {}", e));
                self.status_ok = false;
            }
        }
        // 不一定有内容（例如一开始就报错），避免出现空的回答气泡
        if !thinking.is_empty() || !answer.is_empty() {
            self.messages.push(Turn {
                question: st.question,
                thinking,
                answer,
                thinking_open: false,
            });
            // 表格横向滚动句柄与 messages 对齐（流式阶段的句柄随之转入历史轮次）
            self.table_scrolls.push(st.table_scroll);
            self.selections.push(st.selection);
        }
        self.messages_scroll.scroll_to_bottom();
    }

    /// Ctrl+C：把"当前所选"的 markdown 源码写入剪贴板。
    ///
    /// 输入框聚焦时（`input::Copy` 交由 TextArea 处理）自动让位。选区为空时
    /// 兜底复制最新一条回答的 markdown 源码。
    fn copy_answer_selection(&mut self, _: &input::Copy, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.read(cx).focus_handle.clone().is_focused(window) {
            return;
        }
        // 找最后一条带非空选区的历史回答
        for i in (0..self.selections.len()).rev() {
            let Some(turn) = self.messages.get(i) else {
                continue;
            };
            let sel = self.selections[i].selection.borrow().clone();
            if let Some(sel) = sel {
                let copied = selected_markdown(&turn.answer, &sel);
                if !copied.is_empty() {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(copied));
                    self.status = SharedString::from("已复制所选（markdown）");
                    self.status_ok = true;
                    window.refresh();
                    return;
                }
            }
        }
        // 兜底：最新一条回答的完整 markdown
        if let Some(turn) = self.messages.last() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(turn.answer.clone()));
            self.status = SharedString::from("已复制最新回答（markdown）");
            self.status_ok = true;
            window.refresh();
        }
    }

    // ==================== 渲染 ====================

    fn render_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let status_color = if self.status.is_empty() {
            rgb(0x9ca3af)
        } else if self.status_ok {
            rgb(0x16a34a)
        } else {
            rgb(0xd97706)
        };
        // 清空按钮
        let clear_btn = div()
            .id("clear-btn")
            .px_3()
            .h(px(30.))
            .rounded_md()
            .bg(rgb(0xffffff))
            .border_1()
            .border_color(rgb(0xd1d5db))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(13.))
            .text_color(if self.stream.is_some() {
                rgb(0x9ca3af)
            } else {
                rgb(0x1f2328)
            })
            .cursor(CursorStyle::PointingHand)
            .child("清空")
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut ChatApp, _ev, _window, cx| this.clear(cx)),
            );

        // 表格超宽处理方式切换：自动换行 / 横向滚动
        let table_toggle = div()
            .id("table-mode-toggle")
            .px_3()
            .h(px(30.))
            .rounded_md()
            .bg(white())
            .border_1()
            .border_color(rgb(0xd1d5db))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.))
            .text_color(rgb(0x1f2328))
            .cursor(CursorStyle::PointingHand)
            .child(if self.table_mode == TableMode::Wrap {
                "表格: 换行"
            } else {
                "表格: 滚动"
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut ChatApp, _ev, _window, cx| {
                    this.table_mode = match this.table_mode {
                        TableMode::Wrap => TableMode::Scroll,
                        TableMode::Scroll => TableMode::Wrap,
                    };
                    cx.notify();
                }),
            );

        div()
            .h(px(56.))
            .bg(white())
            .border_b_1()
            .border_color(rgb(0xe5e7eb))
            .flex()
            .items_center()
            .justify_between()
            .px_4()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(0x1f2328))
                            .child("AI 问答助手"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(0x6b7280))
                            .bg(rgb(0xf3f4f6))
                            .px_2()
                            .py_1()
                            .rounded_full()
                            .child("Dify · 文档问答"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .max_w(px(360.))
                            .text_size(px(12.))
                            .text_color(status_color)
                            .child(if self.status.is_empty() {
                                "就绪".to_string()
                            } else {
                                self.status.to_string()
                            }),
                    )
                    .child(table_toggle)
                    .child(clear_btn),
            )
    }

    fn render_input_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let q = self.current_question(cx);
        let can_send = self.stream.is_none() && !q.trim().is_empty();

        let send_btn = div()
            .id("send-btn")
            .px_5()
            .h(px(38.))
            .rounded_lg()
            .bg(if can_send { rgb(0x2563eb) } else { rgb(0x9ca3af) })
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(14.))
            .text_color(white())
            .cursor(CursorStyle::PointingHand)
            .child(if self.stream.is_some() {
                "生成中"
            } else {
                "发送"
            })
            .on_mouse_up(MouseButton::Left, {
                let enabled = can_send;
                cx.listener(move |this: &mut ChatApp, _ev, window, cx| {
                    if enabled {
                        this.send_from_input(window, cx);
                    }
                })
            });

        div()
            .px_4()
            .py_3()
            .bg(white())
            .border_t_1()
            .border_color(rgb(0xe5e7eb))
            .flex()
            .flex_row()
            .items_end()
            .gap_2()
            .child(
                div()
                    .id("input-field")
                    .flex_1()
                    .min_w(px(0.))
                    .max_h(INPUT_MAX_HEIGHT)
                    .overflow_y_scroll()
                    .track_scroll(&self.input.read(cx).scroll_handle.clone())
                    .border_1()
                    .border_color(rgb(0xd1d5db))
                    .rounded_lg()
                    .bg(white())
                    .px_2()
                    .text_size(px(14.))
                    .cursor(CursorStyle::IBeam)
                    .track_focus(&self.input.read(cx).focus_handle.clone())
                    .focus(|this| this.border_color(gpui::blue()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this: &mut ChatApp, _ev, window, _cx| {
                            window.focus(&this.input.read(_cx).focus_handle.clone());
                        }),
                    )
                    .child(self.input.clone()),
            )
            .child(send_btn)
    }

    fn question_bubble(&self, question: &str) -> impl IntoElement {
        div()
            .max_w(px(560.))
            .min_w(px(0.))
            .bg(rgb(0x2563eb))
            .px_3()
            .py_2()
            .rounded_lg()
            .text_color(white())
            .text_size(px(14.))
            .child(SharedString::from(break_long_runs(question)))
    }

    fn render_turn(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let turn = &self.messages[index];
        // 回答区容器：右键（次级鼠标键）整条复制到剪贴板
        let mut answer_block = div()
            .id(ElementId::Name(SharedString::from(format!("answer-{index}"))))
            .min_w(px(0.))
            .on_mouse_down(MouseButton::Right, {
                let target = CopyTarget::Answer(index);
                cx.listener(move |this: &mut ChatApp, _ev, window, cx| match target {
                    CopyTarget::Answer(i) => this.copy_turn_answer(i, window, cx),
                    CopyTarget::Thinking(i) => this.copy_turn_thinking(i, window, cx),
                })
            });
        if !turn.thinking.is_empty() {
            let open = turn.thinking_open;
            let toggle = div()
                .id(ElementId::named_usize("think-toggle", index))
                .mt_2()
                .mb_1()
                .cursor(CursorStyle::PointingHand)
                .text_size(px(12.))
                .text_color(rgb(0x6b7280))
                .child(if open {
                    "▾ 深度思考（点击收起）"
                } else {
                    "▸ 深度思考（点击展开）"
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut ChatApp, _ev, _window, cx| {
                        if let Some(t) = this.messages.get_mut(index) {
                            t.thinking_open = !t.thinking_open;
                        }
                        cx.notify();
                    }),
                );
            // 思考折叠行右侧带"复制"按钮
            answer_block = answer_block.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(toggle)
                    .child(ChatApp::copy_button(
                        cx,
                        ElementId::Name(SharedString::from(format!("copy-thinking-{index}"))),
                        "复制",
                        CopyTarget::Thinking(index),
                    )),
            );
            if open {
                answer_block = answer_block.child(
                    div()
                        .bg(rgb(0xf3f4f6))
                        .border_1()
                        .border_color(rgb(0xe5e7eb))
                        .rounded_md()
                        .p_3()
                        .min_w(px(0.))
                        .text_size(px(12.5))
                        .text_color(rgb(0x4b5563))
                        .child(SharedString::from(break_long_runs(&turn.thinking))),
                );
            }
        }
        // 回答右上角：复制按钮
        answer_block = answer_block.child(
            div()
                .flex()
                .flex_row()
                .justify_end()
                .mt_2()
                .mb_0()
                .child(ChatApp::copy_button(
                    cx,
                    ElementId::Name(SharedString::from(format!("copy-answer-{index}"))),
                    "⧉ 复制回答",
                    CopyTarget::Answer(index),
                )),
        );
        answer_block = answer_block.child(render_markdown_with_options(
            &turn.answer,
            &MarkdownTheme::light(),
            self.table_mode,
            self.table_scrolls.get(index),
            Some(window),
            self.selections.get(index),
        ));

        div()
            .mb_4()
            .child(
                div()
                    .flex()
                    .justify_end()
                    .child(self.question_bubble(&turn.question)),
            )
            .child(answer_block)
    }

    /// 回答右上角：复制按钮
    fn copy_button(
        cx: &mut Context<ChatApp>,
        id: ElementId,
        label: &str,
        target: CopyTarget,
    ) -> impl IntoElement {
        div()
            .id(id)
            .px_2()
            .py_0()
            .rounded_md()
            .text_size(px(12.))
            .text_color(rgb(0x9ca3af))
            .hover(|this| this.text_color(rgb(0x2563eb)))
            .cursor(CursorStyle::PointingHand)
            .child(SharedString::from(label.to_owned()))
            .on_mouse_up(MouseButton::Left, {
                cx.listener(move |this: &mut ChatApp, _ev, window, cx| match target {
                    CopyTarget::Answer(i) => this.copy_turn_answer(i, window, cx),
                    CopyTarget::Thinking(i) => this.copy_turn_thinking(i, window, cx),
                })
            })
    }

    fn render_stream(&self, window: &mut Window) -> impl IntoElement {
        if let Some(st) = &self.stream {
            let mut block = div().mt_2();
            if !st.thinking.is_empty() {
                block = block.child(
                    div()
                        .mb_2()
                        .bg(rgb(0xf3f4f6))
                        .border_dashed()
                        .border_1()
                        .border_color(rgb(0xd1d5db))
                        .rounded_md()
                        .p_2()
                        .min_w(px(0.))
                        .text_size(px(12.5))
                        .text_color(rgb(0x6b7280))
                        .child(SharedString::from(break_long_runs(&st.thinking))),
                );
            } else {
                block = block.child(
                    div()
                        .mb_2()
                        .text_size(px(13.))
                        .text_color(rgb(0x9ca3af))
                        .child("正在深度思考..."),
                );
            }
            block = block.child(render_markdown_with_options(
                if st.answer.is_empty() {
                    " "
                } else {
                    &st.answer
                },
                &MarkdownTheme::light(),
                self.table_mode,
                Some(&st.table_scroll),
                Some(window),
                Some(&st.selection),
            ));
            div()
                .mb_4()
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .child(self.question_bubble(&st.question)),
                )
                .child(block)
        } else {
            div()
        }
    }

    fn render_empty_state(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let sample = div()
            .id("sample-btn")
            .mt_4()
            .px_4()
            .py_2()
            .rounded_md()
            .bg(rgb(0xeff6ff))
            .border_1()
            .border_color(rgb(0xbfdbfe))
            .text_size(px(13.))
            .text_color(rgb(0x2563eb))
            .cursor(CursorStyle::PointingHand)
            .child("示例：文档包含哪些规划项目和周期？请给出完整表格。")
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut ChatApp, _ev, _window, cx| {
                    this.input.update(cx, |input, cx| {
                        input.content = SharedString::from(
                            "文档包含哪些规划项目和周期？请给出完整表格。",
                        );
                        input.selected_range = input.content.len()..input.content.len();
                        cx.notify();
                    });
                    let q = this.input.read(cx).content.to_string();
                    this.start_question(&q, cx);
                }),
            );

        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .text_color(rgb(0x6b7280))
            .child(
                div()
                    .text_size(px(44.))
                    .mb_2()
                    .child("💬"),
            )
            .child(
                div()
                    .text_size(px(16.))
                    .text_color(rgb(0x374151))
                    .child("基于文档的 AI 深度问答"),
            )
            .child(
                div()
                    .mt_1()
                    .text_size(px(13.))
                    .text_color(rgb(0x9ca3af))
                    .child("已加载默认文档，直接输入问题开始对话"),
            )
            .child(sample)
    }

    fn render_messages(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let empty = self.messages.is_empty() && self.stream.is_none();
        let mut scroll = div()
            .id("messages")
            .flex_1()
            .overflow_y_scroll()
            .track_scroll(&self.messages_scroll)
            .px_4()
            .py_3()
            .min_w(px(0.));
        if empty {
            scroll = scroll.child(self.render_empty_state(cx));
        } else {
            for i in 0..self.messages.len() {
                scroll = scroll.child(self.render_turn(i, window, cx));
            }
            if let Some(st) = &self.stream {
                let _ = st;
                scroll = scroll.child(self.render_stream(window));
            }
        }
        // 消息区 + 美化滚动条（并列一行，滚动条占据最右 12px 窄列）
        div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h(px(0.))
            .child(scroll)
            .child(super::scrollbar::Scrollbar::new(self.messages_scroll.clone()))
    }
}

impl Render for ChatApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _theme = MarkdownTheme::light();
        // FPS 调试：每帧时间戳算瞬时帧率（EMA 平滑）；开启时持续出帧以便统计（含空闲）
        if self.fps_on {
            let now = Instant::now();
            if let Some(last) = self.fps_last {
                let dt = now.duration_since(last).as_secs_f32().max(1e-5);
                let inst = 1.0 / dt;
                self.fps_ema = if self.fps_ema <= 0.0 {
                    inst
                } else {
                    self.fps_ema * 0.9 + inst * 0.1
                };
            }
            self.fps_last = Some(now);
            window.request_animation_frame();
        }
        // Ctrl+C：输入框聚焦时由 TextArea 处理；否则复制回答区的当前选区（markdown 源码）
        let view = cx.entity();
        // 鼠标在任意处松开都要清掉选择锚点：防止松开落在文本空隙时锚点残留，
        // 导致之后只移动鼠标也会拉长选区（"不跟手"的一种表现）
        let fps_overlay = if self.fps_on {
            let ema = self.fps_ema;
            let ms = if ema > 0.0 { 1000.0 / ema } else { 0.0 };
            Some(
                div()
                    .absolute()
                    .top_2()
                    .left_2()
                    .px_1()
                    .py(px(2.))
                    .rounded_md()
                    .bg(rgb(0xffffff))
                    .border_1()
                    .border_color(rgb(0xd1d5db))
                    .text_size(px(11.))
                    .text_color(rgb(0x374151))
                    .child(format!("{ema:.0} fps · {ms:.1} ms/帧")),
            )
        } else {
            None
        };
        let root = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0xf3f4f6))
            .track_focus(&self.focus_handle(cx))
            .on_mouse_up(MouseButton::Left, cx.listener(move |this: &mut ChatApp, _ev, _window, _cx| {
                for s in this.selections.iter() {
                    *s.anchor.borrow_mut() = None;
                }
                if let Some(st) = &this.stream {
                    *st.selection.anchor.borrow_mut() = None;
                }
            }))
            .on_action(move |action: &input::Copy, window, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.copy_answer_selection(action, window, cx)
                });
            })
            .child(self.render_header(cx))
            .child(self.render_messages(window, cx))
            .child(self.render_input_bar(cx));
        if let Some(overlay) = fps_overlay {
            root.child(overlay)
        } else {
            root
        }
    }
}

