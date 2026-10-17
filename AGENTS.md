<!--
REFACTORED: 2026-10-17
STYLE: LLM Protocol Body (v2)
LAST_AUDIT_PATCH: 2026-10-17-001
-->

# AGENTS.md（LLM 执行协议 v2）

## 0. 项目与角色边界

**项目**：Rust + GPUI 的编辑器/阅读器应用（`gpui-editor`）。P5 已拆 Cargo workspace：`src/main.rs` 只是薄二进制壳，功能全在各 crate（`crates/shell`=外壳/侧栏/EditorApp、`crates/editor`=编辑区、`crates/markdown`=Markdown 预览、`crates/pdf`=PDF 阅读、`crates/epub`、`crates/viewer`、`crates/image_viewer`、`crates/video`、`crates/syntax`、`crates/ui`、`crates/toc`、`crates/config`、`crates/base`、`crates/ime`）。

**角色职责表**：

| 角色 | 职责 | 边界 |
|---|---|---|
| 主 agent | 派活 → 收报告 → 轻量核对 → 按用户指示提交/推送 | 不读大文件、不精读大 diff、不亲手写功能代码；提交仅当用户明确说"提交git" |
| 子 agent `general` | 实现 / 评审 / 性能对比 / 问题定位（可动手） | 不提交 git |
| 子 agent `explore` | 只读探索 | 不修改任何文件 |

**任务归属**：实现 / 评审 / 性能对比 / 问题定位一律交由子 agent。

**亲为例外**：根因已核实 + 改动小而精确时，主 agent 可直接改（依据 K1；涉及纯函数化/参数语义/调用点时仍须满足 §3「亲为不变式」）。

## 1. 派活协议

### 1.1 派活前强制思考（<thinking> 内逐条作答后才写 brief）

```
<thinking>
A. 三问是否已用一次 question 对齐？用户答案是否逐字抄入 brief
   （构建模式 / 目标可观测行为 / 约束：许可 兼容 默认产物 不能改的语义）？
B. 行为匹配类（排序/格式/交互/路由）brief 是否含「精确可观测期望 + 回归断言」？
C. 性能/耗时类是否已立用户感知基线（如 debug 打开首帧墙钟）？
   方案改变不了该指标 → 是否直接返回阻塞、不发货？
D. 选型/许可类约束是否已确认（新依赖/许可接受度）？
E. 评估类是否已在 brief 明说「ROI 不匹配可报维护现状」？
F. 评估已判「低 ROI/维持现状/不建议」的方向，是否已再用一次 question 复核后才派实现？
   （复核须在**写 brief 之前**完成——P1 教训：brief 写完才被中止，成本已付）
G. 并行性：是否已判定「谁会被别家未提交的中间编译/测试态污染」？
   共享测试套件的 crate 家族 → 一律串行；只读分析 / 不同 crate 独立改 → 可并行。
H. 目标 + 已知落点是否给了 path:行号（附「行号可漂移、先 grep/read 精读再动」）？
I. 需求清单是否逐条可验收？缓存/派生状态改动是否列全失效源？
   交互语义是否已给「各触发点行为表」并用一次 question 确认？
J. 复用：功能/UI 派活前是否已查既有可复用组件？轻量组件要补现有能力是否先确认复用核心？
K. 纪律是否已写入 brief：不改 gpui checkout、不加依赖、提交权在主 agent、
   颜色走 ui::theme、注释中文、先 get_minimal_context_tool(task=...)？
L. 会话复用检查（**必填**，派活前完成，不得省略）：
   1. 列出本次任务核心文件/区域
   2. 检索前序活跃子 agent 会话中是否有「同文件 + 同区域 + 同角色（实现↔实现、探针↔追查）」者
   3. 有 → brief 必须声明 `task_id: <id>` 且只写 delta；无 → 声明「task_id: 无（新建）」，并给一句理由
   4. 实现↔评审跨角色不续跑（保持独立）→ 该情形不算"无复用"，不必续跑
   > ⚠️ 声明"无"但实际存在同文件同角色可复用会话 → 视为违规
</thinking>
```

### 1.2 Brief 必含六字段

| 字段 | 要求 |
|---|---|
| 0 验收口径前置 | 三问答案逐字抄入；行为匹配写成回归断言；性能给用户感知基线 |
| 1 目标 + 已知落点 | `path:行号` + 「行号可漂移，先 grep/read 精读再动」 |
| 2 需求清单 | 逐条可勾选可验收；缓存/派生状态列全失效源（换文档/切预览/内容编辑/整篇换源/折叠/只读切换等）；交互语义给行为表后一次 `question` 确认 |
| 3 纪律 | 不改 gpui checkout、不加依赖、提交权在主 agent、颜色走 `ui::theme`、注释中文、先 `get_minimal_context_tool`、复用既有组件 |
| 4 验证 | 见 §3；窗口生命周期/对话框/持久化类必须含真机 GUI 冒烟项 |
| 5 报告 | `path:line` → 机制 → 验证输出摘要 → 遗留风险 → 建议 commit message（几 KB 内，禁转贴源码） |
| 6 复用或新建声明 | `task_id: <id>`（续跑只写 delta）或 `task_id: 无（新建，一句话理由）`；声明"无"但存在同文件同角色可复用会话 → 违规 |

## 2. 流水线

```
主 agent 轻侦察(grep 定落点) → 实现子agent(brief) → 评审子agent(发现缺陷直接修)
→ 主 agent 轻核对(git status --short && git diff --stat) → 用户说"提交git"才提交
```

### 2.1 修 bug 流程

- 复现测试先行：真实渲染断言，先 FAIL 修到 PASS。
- 根因由实现子 agent 自证（引用代码证据）。
- Brief 根因字段：只写已核实根因（主 agent read 确证过才给，直接省实现时间）；未核实猜测只作线索并标注「需子 agent 核实」。
- bug 紧跟近期改动后报告 → 先定性「回归 or 既有」：首选 `git show <上一commit> -- <涉事文件>` 只读确认改动是否碰过相关行；仅当确需真跑旧行为才 `git stash`（先存 diff 指纹精确还原）再修。

### 2.2 评审规则

| 场景 | 执行 |
|---|---|
| 逻辑 / 几何 / 命中 / 并发类 | 实现 + 评审双 agent |
| 纯视觉 / 数值微调（k/曲率/间距等） | 实现侧自验即可，不派评审 |
| 同文件连续微调 | 用 `task_id` 续跑同一子 agent 会话；续跑仅限**同角色**（实现↔实现、探针↔探针/追查），实现↔评审保持独立会话（防偏见）；续跑轮 brief 只写 delta |
| 评审视角 | 正确性 + 性能双视角、与实现同轮收尾；评审子 agent 带性能眼（热路径/复杂度/磁盘 IO 量级/占用），发现热点先量化再改，说明「更快但破坏语义」方案的取舍理由 |
| 评审分级 | 高风险共享模块（editor/view/core/shell/公共组件）亲自重跑；低风险单模块只读 diff + 抽查关键断言 + 定向跑，与实现方共用同一份测试输出 |
| 平台语义断言 | 实现方声称的「命中互斥成立 / 事件不穿透 / 资源释放遍历范围」等 gpui 平台语义默认作**待证项**：评审须读 gpui 源码或写探针实证后才通过（K14/K15 实证过，反例：双层覆盖"优先消费"、drop_image 漏当前窗口均靠评审实证推翻） |
| 提交前提 | 实现 + 双评审通过 → 先评审后提交；提交权在主 agent，须用户明确说"提交git" |

### 2.3 并行规则

| 场景 | 执行 |
|---|---|
| 同文件改动 | 强制串行 |
| 共享测试套件的 crate 家族（如 shell 测试吃 markdown/editor 编译态） | 一律串行 |
| 只读分析 / 不同 crate 独立改动 | 可并行 |

- 并行前先判定「谁会被别家未提交的中间编译/测试态污染」（依据 K7）。

### 2.4 派生状态副作用

- 会话/缓存类状态更新埋点：采用「一处脏标记 + 统一 flush」，禁止散点直调（散点漏触发=丢状态、滥触发=热点）。
- 改动前明确列出全部失效源。

## 3. 验证与编译协议

| 规则 | 触发条件 / 执行 |
|---|---|
| 构建一律 debug | 所有构建/测试/测量；禁止 release |
| workspace 测试带 crate | 一律 `cargo test -p <crate>`（根目录裸 `cargo test` 只盖根包+video-ffmpeg）；pdf 加 `--test-threads=1` |
| 改动完再编译 | 每组改动改完一次编译、复用产物连跑多组；`cargo check` 并入 `--tests` 或省略；实现侧压到 ~2 轮；大 crate（如 shell）一次改完再编、共享一次性能测量 |
| filter 用模块路径前缀 | 如 `cargo test -p shell app::tests::sidebar` 一次盖全模块；核心渲染改动也按模块前缀收最小集，仅共享核心/用户要求才放宽全模块 |
| 验证范围 | 只做改动部分；全量仅当用户明确要求/特性定型 |
| stash 基线 | 行为功能不跑（回归由评审兜底）；仅两类场景允许：① 性能对比子 agent before/after（pop 后逐文件核对还原）② 疑似预存失败硬定性（先存 diff 指纹精确还原） |
| 边界测试先行 | 边界清单逐项打勾（非行 0、同行多元素、折行续行、文档结尾、空文档等），实现侧勾完才算完成 |
| 视觉/渲染完成门槛 | 画面/UI 改动报「完成」前必须 `cargo build --bin gpui-editor` + 真机冒烟（截图/pixel 验证，复用 `feature-delivery`/`gpui-open-scroll` 探针）；做不到运行时验证 → 报告固定写「已实现+单测过、未冒烟」 |
| 用户负面报告 | 用户报「没有看到 X / 行为不对 / 没生效」→ 第一回应先取证：exe 时间戳（`Get-Item target\debug\gpui-editor.exe` 比源文件新）、真机复现/探针/像素定位，再下结论 |
| 亲为不变式 | 亲为改动涉及纯函数化/参数语义/调用点 → 逐参对照函数签名（形参语义「X=true 表示…」逐个写死）+ 一条运行断言；否则计入常规「实现+评审」流 |
| 长命令后台化 | 预计 >2min（历史重写/长 build/大测试）→ `background_process start` + `schedule_wakeup` 轮询；logs 只取尾部 ≤20 行（禁整屏 `Rewrite i/N` 进度刷屏） |
| 运行时冒烟采样 | 窗口生命周期/对话框/持久化类 brief 必含真机 GUI 冒烟项（复用 feature-delivery 探针，新探针沉淀回技能）；平台钩子（`on_app_quit`/`on_window_should_close` 等）先实测再采信 |
| 性能红线 | 用户动作同步路径不得引入大主线程 IO/大内容重读；持久化/派生状态写盘按复杂度上限评估（hash 跳过、粒度合并）；把 N ms 级主线程停顿当候选热点列明 |

## 4. 主 agent 上下文收敛

| 动作 | 执行 |
|---|---|
| 侦察下放 | 派活前只做 1-2 条 CRG 查询（`get_minimal_context_tool` / `query_graph` / `traverse_graph`）或 grep 定入口；不 read 实现细节、不跑多路 grep/采样替子 agent 摸底；派活只需「结构入口 + 参考实现」，不追 paint/hit 具体行号 |
| 核对极简 | 收报告后只跑一条 `git status --short && git diff --stat`；确需抽查至多 1 个 grep |
| brief 收敛 | 只写需求+约束+已知落点；根因只写已核实、不写猜测；续跑轮只写 delta；歧义合并一次 `question`；bench/大表输出一律 `-Last N`/limit 截断 |

## 5. 子 agent 验收口径（别当"回归"）

| 项 | 口径 |
|---|---|
| crate 测试 | `cargo test -p <crate>` 全绿；测试数随迭代波动，不锁旧计数 |
| pdf/video 夹具 | 仓库根 `target/pdf测试.pdf` + `target/pdf测试2.pdf`、`target/视频测试.mp4`（不入库、手动装配；crate 经 `CARGO_MANIFEST_DIR/../../target` 定位）；缺夹具打 `SKIP: missing target/...` 后 return——SKIP 是常态不是回归 |
| 测试环境 | 窗宽抖动；debug ≈ release 的 ~10×；看相对变化与 stable 子项，别看绝对值；数据处理用 python；rg 不一定在 PATH → 用 grep/glob |

## 6. 经验知识库（原理与教训，正确做法 + 错误后果）

| # | 知识条目 |
|---|---|
| K1 | 亲为例外有效：排序「原始 CompareStringEx」与「GIF 失焦自驱时钟」主 agent 亲为一次成功；子 agent 曾「剥符号改乱顺序」「误读 syntect API」产生失真 → 亲为只在根因已核实 + 改动小而精确时采用，子 agent 失真成本比主上下文成本更贵 |
| K2 | 行为匹配要回归断言：子 agent 自作主张改 UX 语义（剥符号）导致整包撤销 → 精确可观测期望写死进 brief |
| K3 | 性能方案：打开大图只降内存不降墙钟导致整包撤销 → 先立用户感知基线 |
| K4 | 选型许可：AGPL zenavif 被要求放弃 → 新依赖先确认许可接受度 |
| K5 | 视觉断言：徽标调用点 `is_none()` 传入有错参双重取反致永不挂出，被误判 stale exe 白耗一轮 → 完成前 build+冒烟、负面报告先取证 |
| K6 | 长命令：filter-branch 首跑 600s 超时被杀、换后台重跑白耗 ~10min → 后台化 + 进度截断 |
| K7 | 并行测试污染：两个实现 agent 并行，shell 测试反复被「预存失败 + 中途编译态」污染，评审轮次全赔在洗白非回归上 |
| K8 | 测试噪声：真实 `%APPDATA%\gpui-editor\config.json` 经 config crate 泄漏进 shell 测试（font/preview 断言失败）、`fixtures/测试.md` 工作区 CRLF——先 `git stash` 基线定性非回归，勿当新回归 |
| K9 | 平台钩子：本平台最后一个窗口关闭退出不回调 `on_app_quit`（`App::quit → platform.quit` 直接结束进程）；退出/关闭类钩子先实测再采信 |
| K10 | 副作用集中注入：会话/缓存更新散点直调曾造成逐次切换全量重读/重写槽位（27–58ms 热点） |
| K11 | 磁盘 IO：持久化/日志类写盘用 hash 跳过 + 粒度合并，别每次全量重写 |
| K12 | 大文档性能红线：不得引入持续逐帧重绘（弹簧收敛即静止）；状态栏等只读派生须 O(1) |
| K13 | 6 个 shell 预存失败靠 stash 基线硬定性，省掉洗白非回归 → 实证比口头断言硬 |
| K14 | gpui 窗口上下文资源释放：`App::drop_image(image, None)` 只遍历 `App.windows`，而窗口自身 key/button 回调在 `update_window_erased` 里把当前窗口**临时移出** `App.windows`（app.rs:3146 take/3190 放回）→ 窗口上下文内当场调用会漏当前窗口（每手动切一次漏一片）；须 `cx.defer` 延后到本拍更新末尾 flush（content_sync `pending_math_drop` 同款姿势） |
| K15 | gpui 鼠标事件沿命中栈**冒泡**（无鼠标监听的覆盖浮层不 BlockMouse，不拦截事件）：双击全屏挂在 size_full 的 `#video-main` 时，控制条/进度条/toast 上的双击会一路漏下来误触——「上层覆盖元素优先消费」是错误直觉；互斥必须在专属层 `stop_propagation`（`click_count>=2` 才拦，单击放行），评审实测「seek 条中点双击 → locked=true」确诊 |
| K16 | 中文文件编码事故：子 agent 用 bash/文件写操作改含中文注释的源码，PowerShell 重编码致**整文件 mojibake**，被迫 `git restore` 后重做（白耗一轮）；改中文源码一律只走 read/edit/write 工具；bash 回显中文乱码时改用 read 以 UTF-8 读 |
| K17 | gpui 测试平台 `toggle_fullscreen`/`is_fullscreen` 是**真实翻转**（`platform/test/window.rs` 同步实现）→ 全屏进出相关单测两路都真跑，非假绿 |

## 7. 已踩坑速查（实现要点）

- 正文卡包装层必须是 flex 容器，否则 `flex_1` 高度坍缩毁滚动/命中。
- `deferred(anchored(Window))` 定位用 `.position(窗口坐标)`，不用 `.offset(...)`（会顶到窗口底/沉底）。
- 欢迎页/零 tab：`tabs` 允许为空，凡 `tabs[active_tab]` 消费一律 `tabs.get(...)` 兜底。
- `with_spring`/`hover_spring`：临界阻尼免过冲；`reduce_motion` 由 gpui 自动尊重。
- gpui 鼠标事件沿命中栈冒泡：无鼠标监听的覆盖浮层不拦截事件；需事件互斥（双击全屏 vs 控制条等）时在专属层 `stop_propagation`（按 `click_count>=2` 拦、单击放行）。（K15）
- gpui 窗口上下文资源释放：`App::drop_image(_, None)` 在窗口回调内漏当前窗口，须 `cx.defer` 延后到本拍更新末尾；同款见 content_sync `pending_math_drop`。（K14）
- 中文源码改动：只用 read/edit/write 工具写文件；禁止 bash/文件写操作（PowerShell 重编码致整文件 mojibake）；bash 回显中文乱码改用 read UTF-8 读。（K16）

## 8. 工具与技能调度

### 8.1 CRG 工具链

- 入口：每个任务开头先 `get_minimal_context_tool(task=...)`，拿风险分、相关 communities/flows、`head_matches_build`。
- 查影响面：优先 `query_graph`/`traverse_graph`（带深度 + token 预算），不手工通读大文件。
- 新鲜度：本机 `code-review-graph watch` 常驻自动增更（仓库根 `.code-review-graph/` 不入库）；图暂停过/换 checkout 后先 CLI `build` 拉新再查；`get_minimal_context`/搜索报 stale 且排查对象不在近期改动里 → 直接用单 token 搜索 + grep 寻路，不追更（`update` 报 0 常因增量基准未随 HEAD 推进；确需拉新用 `build`）。
- 调度引用：复杂任务/评审/重命名/架构体检前先 `skill` 加载 `code-review-graph`（`.kilo/skills/`，自动发现），按其已验证调用姿势执行；功能交付/交互语义/运行时冒烟前先 `skill` 加载 `feature-delivery`（语义确认表、brief 模板、双评审、探针用法；`.kilo/skill/`，自动发现）。

### 8.2 CRG 硬性纪律（违反会得到过期/错误结论）

六条：① 新鲜度（`head_matches_build`≠dirty、增量 summary 不可信、`full_rebuild` 剔未跟踪）② 未提交评审必配 `base="HEAD"` + `changed_files` ③ 搜索用单 token、别 kind 过滤、函数体走 grep ④ `query_graph` target 用裸短名/qualified_name，点分名 not_found ⑤ `dead_code` 有假阳性（BracketPair），删前 grep 复核 ⑥ `get_flow`/`get_community` 别传 `detail_level`、flow 用 name 查。

完整姿势与「纪律↔技能节」对照表见 skill `code-review-graph`（§9 外部索引；坑速查 §7）。

### 8.3 常规循环

日常 探索/评审/重命名/架构体检 的调用序列见 skill `code-review-graph`「§8 推荐日常节奏」；落地改动与回归仍以 `read`/`grep` + `cargo test` 为准，图不替代码精读与编译验证。

## 9. 多仓库依赖图（gpui zed）

gpui/zed 已注册建图（`~/.code-review-graph/registry.json`）；跨仓检索 `cross_repo_search_tool("<裸短名>")`、单仓 `query_graph`/`semantic_search_nodes` 传 `repo_root` 等完整姿势见 skill `code-review-graph` §6.3。gpui checkout 路径运行时解析（rev 从 `Cargo.lock` 读、目录用 7 位短 rev）：

```powershell
$base = Join-Path $env:USERPROFILE ".cargo\git\checkouts\zed-a70e2ad075855582"
$rev  = [regex]::Match((Get-Content Cargo.lock -Raw), 'zed\?rev=([0-9a-f]{40})').Groups[1].Value.Substring(0, 7)
$gpui = Join-Path (Join-Path $base $rev) "crates\gpui"   # 即 repo_root
```

建图/状态日常走 MCP 工具（`build_or_update_graph_tool`/`list_repos_tool` 等），CLI 仅 register/用本机 venv（`.kilo/kilo.jsonc`，不入库）；cargo 更新换 checkout sha 后旧路径失效，必要时重新 `register` + `build`。
