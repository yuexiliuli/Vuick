# gpui-ai-chat

基于 **Rust + GPUI 0.2.2** 原生实现的"AI 文档问答"桌面聊天工具，是其 Tauri 成品的 GPUI 重构版。
界面为"标题栏 + 可滚动消息区 + 底部多行输入栏"三栏布局，网络逻辑全部放到后台线程，主线程只做渲染与事件。

## 功能特性

- **深度问答**：启动时把内嵌的加密默认文档（`assets/doc.bin`）解密后上传，换取 `upload_file_id`；问答走 Dify 流式（SSE），`深度思考 / 回答` 实时增量渲染，返回后写入历史轮次。
- **深度思考折叠**：思考内容可展开/收起，右侧带"复制"按钮。
- **Markdown 渲染**（`markdown.rs`，移植自前端 `markdown.ts`）：
  - 段落、标题、行内 `code` / 加粗 / 斜体 / 删除线 / 链接、围栏代码块、有序/无序列表、表格、引用、分割线；
  - **表格双模式**：默认"自动换行"；超宽可切换"横向滚动"——各列按内容实测宽度对齐，底部出现**可拖拽的横向滚动条**，滚动时滚轮锁定在表格内、不再联动页面；
  - 表头加粗真正作用到 TextRun 字体；长不可断片段（长 URL/长串）自动插入零宽空格换行，不再溢出窗口。
- **内容选择与复制**（GPUI 0.2.2 无内置静态文本选择，为自研实现）：
  - 鼠标**拖选**回答正文，选区**实时高亮**（跨段落/标题/列表/代码/表格连续选中）；
  - `Ctrl+C` 复制**所选部分的 markdown 源码**（字符级裁剪：完整选中的 `**加粗**`/`` `code` `` 保留标记，部分选中只取纯文本，避免半截标记）；
  - 回答右上角"⧉ 复制回答"、思考折叠行"复制"、回答区**右键**均可把整条 **markdown 源码**写入剪贴板（不再是纯文本）；
  - 输入框聚焦时 `Ctrl+C` 照常复制输入内容（自动让位）。
- **多行输入框**（`input/text_area.rs`，从零实现）：Enter 发送、Shift+Enter 换行；撤销/重做、词级/文档级导航、中文 IME 组合输入；空态占位符居中；上千行文档打字走**行级增量排版**（未变化的行整行复用，中段打字只重排受影响的行），滚动后点击位置不错位。
- **美化滚动条**（`scrollbar.rs`）：消息区右侧自绘垂直滚动条；表格下方可拖拽横向滚动条。
- **会话管理**：顶部状态提示（连接/上传/失败），"清空"按钮重置会话。
- **敏感配置外置**：API 地址、密钥等放在 `assets/config.json`（已 `.gitignore`），编译期 `include_str!` 嵌入，运行期不落盘。

## 技术栈

| 类别 | 选型 |
| --- | --- |
| 语言 / UI | Rust · GPUI `0.2.2`（无 webview，原生窗口） |
| 后端 HTTP | `reqwest` blocking / json / multipart |
| 序列化 | `serde` / `serde_json` |
| 文本处理 | `regex` · `unicode-segmentation` |
| 加解密 | `aes-gcm` · `hex` |
| 测试 | GPUI `test-support`（`cargo test`） |

## 目录结构（主要代码作用）

```
src/
├── main.rs                     # 入口：初始化应用、注册键盘快捷键、打开主窗口
├── app.rs                      # 主视图 ChatApp：标题栏/消息区/输入栏布局，历史轮次与
│                               #   流式状态管理，复制(按钮/右键/Ctrl+C)、空态、清空会话
├── backend.rs                  # Dify 后端客户端：解密上传文档、流式对话(SSE)、会话维护
│                               #   配置从 assets/config.json 编译期嵌入（BackendConfig）
├── markdown.rs                 # Markdown → GPUI 元素树渲染 + 文本选择子系统
│                               #   · 解析(parse_markdown/行内 spans)、渲染(表格双模式 断行)
│                               #   · 选择(SeclectCtx/SelectText 自绘叶子)：拖选高亮、
│                               #     命中测试(行尾也算)、选区→markdown 源码还原
├── scrollbar.rs                # 自绘滚动条：垂直(Sceollbar) + 表格横向(horizontal_scrollbar)
└── input/
    ├── mod.rs                  # 模块导出与默认动作集
    ├── shared.rs               # 共享工具：词边界、选区矩形等纯函数 + actions! 动作声明
    ├── text_area.rs            # 多行输入控件（TextArea，行级增量排版/输入法/选择/滚动）
    └── text_box.rs             # 单行输入预留组件
assets/
├── config.json                 # 后端敏感配置（gitignore，编译期嵌入，勿提交）
└── doc.bin                     # 默认加密文档（编译期嵌入，启动时解密上传）
```

### 核心机制说明

- **行级增量排版**（`text_area.rs`）：每个逻辑行缓存 `Arc<WrappedLine>`，内容变更时前后缀逐行复用、仅重排中段；排版缓存只归 prepaint 阶段维护，measure 阶段只读；`canonical_font_size` 统一两阶段字号，避免"底部空白"。
- **表格横向滚动**（`markdown.rs` + `scrollbar.rs`）：列宽用 `text_system.shape_line` 实测统一；滚动容器为 flex 行 + `track_scroll`，超宽时表格可横向溢出，滚动条为可拖拽 div 组件；滚轮在可滚动时 `stop_propagation()` 防止联动页面。
- **文本选择**：每条回答一个 `SelectCtx`（跨帧共享），文本"流"坐标贯穿整条回答；每个文本叶子是自绘 `SelectText`（用 GPUI 精确的 `index_for_position`/`position_for_index` 做命中与高亮，天然支持行尾与换行）；排版结果跨帧缓存，拖动时每帧只更新坐标，保证跟手。
- **流式渲染**：后台线程 `mpsc` 推流，主线程 10ms 轮询刷新；表格横向位置/选区在流式阶段与结束后的历史轮次间平滑迁移。

## 快捷键

| 按键 | 作用 |
| --- | --- |
| `Enter` | 发送 |
| `Shift+Enter` / `Ctrl+Enter` | 换行 |
| `Ctrl+C` | 输入框聚焦→复制输入内容；回答区聚焦→复制所选（markdown 源码） |
| `Ctrl+V` / `Ctrl+X` | 粘贴 / 剪切 |
| `Ctrl+Z` / `Ctrl+Y` | 撤销 / 重做 |
| `← → ↑ ↓`（可加 `Shift`/`Ctrl`/`Alt`） | 光标移动 / 选择 / 词级与文档级导航 |
| `Ctrl+Q` | 退出 |

## 构建与运行

```bash
# 运行（开发）
cargo run

# 发布
cargo build --release

# 全量测试（含文本域、Markdown、滚动、选区等回归）
cargo test
```

> 说明：`assets/config.json` 被 `.gitignore` 忽略。克隆后需要自行准备该文件（字段见下）与 `assets/doc.bin`（可选加密默认文档），否则 `include_str!` 编译会失败。

## 配置

`assets/config.json` 编译期嵌入，字段（`BackendConfig`）：

```jsonc
{
  "api_base":      "https://<dify-host>/v1",   // Dify API 根地址
  "api_key":       "app-xxxx",                 // 应用密钥
  "default_user":  "anonymous",                // 默认用户
  "url_upload":    "/files/upload",            // 文档上传路径
  "url_chat":      "/chat-messages",           // 对话接口
  "field_getsecret": "...",                    // 解密密钥来源字段
  "field_file_type": "pdf",                    // 默认文档类型
  "field_file_transfer": "..."                 // 上传文件字段名
}
```

## 安全备注

- 密钥只外置到 `assets/config.json`（不入库），但**客户端内嵌密钥结构性不安全**：`strings` 可在可执行文件中直接挖出明文。生产环境应改为"客户端不带密钥、经自有代理转发鉴权"的方案，并定期轮换。
- 该说明同样适用于解密上传的默认文档（`doc.bin`）——解密所需密钥存在于客户端，文档内容可被提取。

## 已知限制与后续方向

- 内容区拖选是整条回答维度的文本选择；图表/图片等富媒体尚未支持选择。
- GPUI 0.2.2 无内置静态文本选择组件，当前为自研方案；后续可参考 gpui-component `TextView SelectionFormat::Source` 思路扩展（如"复制纯文本"切换、双击选词）。
- 长聊天下若仍感觉拖动略重，可引入"渲染计划缓存"：把每轮 `parse_markdown` + 叶子样式数据按回答文本缓存，帧循环只重建元素树。
