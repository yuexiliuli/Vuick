# AGENTS.md

## 项目

Rust + GPUI 的编辑器/阅读器应用（`gpui-editor`）。P5 已拆成 Cargo workspace：`src/main.rs` 只是薄二进制壳，功能全在各 crate（`crates/shell`=外壳/侧栏/EditorApp、`crates/editor`=编辑区、`crates/markdown`=Markdown 预览、`crates/pdf`=PDF 阅读、`crates/epub`、`crates/viewer`、`crates/image_viewer`、`crates/video`、`crates/syntax`、`crates/ui`、`crates/toc`、`crates/config`、`crates/base`、`crates/ime`）。

## 上下文保护：子 agent 编排与全代码周期自动化（主 agent 上下文保护）

**主 agent 上下文要省着用**：铁律——主 agent 只做「派活 → 收报告 → 轻量核对 → 按用户指示提交/推送/调度」，一切实现/评审/性能对比/问题定位都用子 agent（`task` 工具：`general`=可动手的多步任务，`explore`=只读调研）；主 agent **不读大文件、不精读大 diff、不亲手写功能代码**。

- **必须委派**：跨模块/跨仓的大范围搜索与整理（如"找出所有调用 X 的地方及其行为差异"）、2000 行以上整文件精读、需要并行做的独立子任务，以及**所有功能实现、评审、性能对比、bug 定位**。
- **例外**：单文件小改动、依赖严格决策链的精确编辑等，若委派明显更慢/更易失真才考虑主 agent 亲自动手；铁律优先。
- **委派姿势**：
  - prompt 里给足**背景、文件路径、明确边界**（"只看 crates/markdown/src/parse/，不要深入 view/wysiwyg"），并写明**要返回的精炼格式**（如"每个调用点：file:line + 一句话行为 + 是否风险"，禁止整段转贴源码）。
  - 多个互不相干的调研/功能用**并行子 agent**；结果回来后主 agent 用 read/grep 抽查要点，不做全量重读。
  - 子 agent 结论是**候选信息**；落地与回归的最终核对见下方"流水线"（评审子 agent + 主 agent 轻量核对）。
- **与 CRG 的关系**：本仓库探索主路径仍先走 CRG 工具（找得快、看得全）；CRG 覆盖面不足的整仓/跨仓大扫描再丢给子 agent。

### 一、任务包工（每次派活必须写全的 brief 模板）

每个子 agent prompt 固定含：
1. **目标 + 已探明落点**：给出手头侦察到的 `path:行号`（附"行号可漂移、先 grep/Read 精读再动"），让子 agent 不从头摸。
2. **需求清单**：可勾选、逐条、可验收。
3. **纪律（写死这几条）**：
   - 不改 gpui checkout、不加依赖；
   - **不提交 git**（提交权在主 agent，且须用户明确说"提交git"）；临时测量代码用完必须原样还原；
   - 颜色走 `ui::theme` token、不散落 rgba；注释中文、与仓库风格一致；
   - 先 `get_minimal_context_tool(task="...")`（子 agent 环境若无 CRG MCP，则 `git diff` + grep/read 精读兜底）。
4. **验证（必须真跑、给输出）**：`cargo check --bin gpui-editor`（0 error / 0 warning）+ 指定最小测试集（一次 `cargo test -p <crate>` 多 filter 合并编译）+ 为新行为补真实渲染路径回归测试——**必须含边界/非首例位置**（非行 0、同行多元素、折行续行、文档结尾等；盲区留到评审才暴露=返工，会多耗一轮 FAIL→PASS 编译）。
5. **最终报告格式**：改动文件清单 `path:line` → 机制/方案 → 验证输出摘要 → 遗留风险 → **建议 commit message**。

### 二、全代码周期流水线（每个功能/修复统一走）

```
主 agent 轻侦察(grep 落点) → 实现子agent(brief) → 评审子agent(git diff+回归，发现问题直接修)
→ 主 agent 轻核对(git status --short + git diff --stat) → 用户说"提交git"才提交
→ (可选)性能对比子agent → (可选)schedule_wakeup 定时推送
```

- **评审子 agent** 对照 checklist：行为保真、几何/坐标、命中/焦点/事件、theme 纪律、无 dead_code/警告、回归名单不变；发现缺陷直接修并重跑。
- **性能对比子 agent**：`cargo test -p markdown markdown::wysiwyg -- --ignored --nocapture`，`git stash push` 切基线 before/after 各 ≥3 跑取中位数+min，`git stash pop` 后**逐文件快照核对工作树还原**（本项目历史教训：P3-12 曾为此丢/乱改动）。
- **修 bug 也走同流水线**：复现测试先行（真实渲染路径断言），定位根因（引用代码证据），修完锁回归。

**流水线提速要点（2026-10-13 实测：子 agent 每轮耗时主因是编译 ×2–3 重复 + 真机反馈闭环）**
- **并行拿红利**：互不冲突的独立子任务**必须并行派**（跨模块/跨文件）；只有同文件改动才强制串行。编译是最大单项，并行能让吞吐翻倍。
- **评审复跑分级，不无脑全量重跑**：①高风险/共享模块改动（editor/view、core、shell 层、公共组件）→ 评审子 agent 仍**亲自全量重跑**；②低风险单模块改动 → 评审子 agent 只要「给实现方的测试输出 + 抽查关键断言 + 跑一次真实失败判别（临时回退修复断言 FAIL）」即可，全量套件只跑 1 次（实现侧），评审侧不重复跑同强度。
- **复现测试合并编译循环**：同一提交内先写复现测试跑 FAIL，立即修复同文件后跑 PASS，再一次性跑相关套件——避免「复现测试编译一次 + 修复后又全量一次」的两段式重复编译。实现与评审子 agent 共用同一份测试输出，别各自起三遍 `cargo test`。
- **主 agent 少写、子 agent 少搜**：brief 里落点给足（`path:line` + "行号可漂移"提示），并明确"不要重新发现、只核实"，压缩冷启动重读成本。

**编译次数收敛（2026-10-14 行内图任务复盘定案：39 分钟内 5-6 轮 debug 编译是大头，行为功能应压到 ~2 轮）**
- **行为类功能取消 stash 基线**：只有**性能对比子 agent** 才 `git stash` 切基线 before/after；行为/功能改动不再跑「改动前基线」（回归由评审子 agent 独立兜底），省 1 轮完整编译。
- **每次编译合并**：同 agent 的一组改动**全部改完再编译**，别改一处编一次；`cargo test` 一次带多个 filter 单次编译跑多组（如 `cargo test -p markdown wysiwyg::element::tests::image inline_image -- --test-threads=1`）；`cargo check` 并入测试（`--tests`）或不单独起。实现侧压到 ~2 轮、评审侧只跑自己改动触发的必需轮。
- **边界测试先行**：实现 brief 强制覆盖**非首例/边界位置**的真实渲染回归（非行 0、同行多元素、折行续行、文档结尾等）——把字节偏移/几何盲区消灭在实现侧，评审才不用再补 FAIL→PASS 的额外编译轮。
- **双 agent 保留但各自减负**：逻辑/几何/命中/并发类改动继续 实现→评审 双 agent（2026-10-14 实测评审抓到「行内相对 vs 全局字节偏移」真 bug，正收益）；评审以「读 diff + 抽查关键断言 + 只跑自己改动触发的测试」为主，不复跑实现侧同强度全量。

### 三、上下文保护技巧（实测有效）

- 主 agent 只跑轻量命令：`git status --short` / `git diff --stat` / `git log --oneline -N` / `git push`；**不 read 大文件、不 git diff 全量**。
- 让子 agent 报「建议 commit message」与几 KB 摘要；主 agent 不追实现细节。
- 一处修改 = 一个子 agent；多项独立功能可顺序/并行派，**同文件改动必须串行**（防冲突）。
- **验证只做改动部分、禁止全量**（2026-10-14）：子 agent 只跑「改动文件所在 crate 的最小精确测试集」（如 `cargo test -p markdown markdown::wysiwyg::mermaid -- --test-threads=1`、或按测试名 filter，**workspace 测试必须带 `-p <crate>`**，根目录裸 `cargo test` 只覆盖根包+video-ffmpeg），**不跑 `markdown::`/`editor::`/`shell::` 全量**；`cargo check` 也在改动后只在需要确认可编译时跑（可与 `--tests` 合并），不要再轮询全量回归；全量回归仅当用户明确要求或特性定型时由主 agent 决定跑一次。
- **复用子 agent 会话加速**：同一文件/特性的连续微调轮用 `task_id` 续跑同一子 agent（跳过冷启动重读），不要把每次微调都新派冷 agent；纯视觉/数值微调轮（调 k/曲率/间距等）**不再单独派独立评审 agent**（自验即可），只有**逻辑/几何语义/命中交互/并发竞态**类改动才走 实现→评审 双 agent。
- **主 agent 侦察/核对/brief 三收敛（2026-10-14 实测定案，主上下文涨速主因）**：
  - 侦察下放：派活 brief 里写「先 grep/read 精读落点再动」（子 agent 本就要读），主 agent 只在派活前做 **1 条** grep 定位；**不 read 大文件、不跑多路 grep/采样/阅读来"替子 agent 摸底"**。
  - 核对极简：子 agent 报告已含 `path:line`+验证输出，主 agent 收报告后只跑**一条** `git status --short && git diff --stat`；不再逐路 grep/read 复核其行号/符号（确需抽查至多 1 个 grep）。
  - brief 收敛：只写「需求 + 约束 + 已知落点提示」，**不写完整实现规格**（实现细节留给子 agent，避免主 agent 精读核心代码）；复用 session 的续跑轮只写 delta。
  - 输出截断：bench/采样/大表工具输出一律 `-Last N`/limit 截断，结论自己消化，展开的整份输出会永久留在主上下文。
  - 一次问清：一个需求的所有歧义点合并进**一次** `question` 调用，不逐点往返。
- 需求有歧义/破坏性（如改键位、改交互模型）先向用户确认一点再派活（本项目用过 `question`，效果好）。

### 四、既有验收基线（子 agent 验证口径，不得把这些当"回归"）
- workspace 测试按 crate 各自全绿：`cargo test -p <crate>`（根目录裸 `cargo test` 只覆盖根包+video-ffmpeg，因 `default-members = [".", "video-ffmpeg"]`，不碰各 crate）；`pdf` 用 `--test-threads=1`。测试数随迭代波动，以当前 `cargo test -p <crate>` 为准，别把旧计数当固定基线。
- pdf 夹具 = 仓库根 `target/pdf测试.pdf` + `target/pdf测试2.pdf`（不入库、需本地手动装配；crate 经 `CARGO_MANIFEST_DIR/../../target` 定位）。缺夹具时 pdf 测试打 `SKIP: missing target/pdf测试*.pdf` 后 return——**SKIP 是常态不是回归**，别误判为失败。
- 测试窗宽度会抖动；debug 数值 ≈ release 的 ~10×；别拿 bench typing-cycle 总数当绝对结论，看相对变化与 stable 子项。
- **只跑 debug、禁止 release 编译**（用户明确要求：本机 release 编译很慢）。perf 一律 `cargo test`/bench 的 debug 相对变化；验证命令模板 `/ 性能对比子 agent` 中不准出现 `--release`。
- rg 不一定在 PATH → 用 grep/glob 工具；shell=pwsh7（`&&` 可用）；数据处理/统计用 python。

### 五、已踩坑速查（避免子 agent 重蹈）
- `justify_content`/flex 布局：正文卡包装层必须是 flex 容器，否则 `flex_1` 高度坍缩毁滚动/命中（外壳卡片化时已锁回归）。
- `deferred(anchored(Window))` 定位：必须用 `.position(窗口坐标)` 而非 `.offset(...)`（offset 是相对锚定元素静态原点的增量，包进 flex 盒子后会把菜单顶到窗口底——选项卡栏右键菜单曾沉底）。两处右键菜单都应 `.position(pos)`。
- 欢迎页/零 tab 模型：`tabs` 允许为空，凡 `tabs[active_tab]` 消费一律 `tabs.get(...)` 兜底。
- `with_spring`/`hover_spring`：临界阻尼免过冲（P2-10 统一），`reduce_motion` 由 gpui 自动尊重。
- 大文档性能红线：任何改动不得引入持续逐帧重绘（弹簧收敛即静止）；状态栏等只读派生须 O(1)。

## 代码探索与评审：优先用本机 CRG 工具链

本仓库开发时必须使用 code-review-graph（CRG）图工具作为调用关系/影响面/评审的首选手段，**不要为了查"谁调用谁 / 影响面 / 改了什么影响的函数"去手工通读 2000 行整文件**。

- MCP server `code-review-graph` 已在 `.kilo/kilo.jsonc` 启用（本地 venv，仓库根 `.code-review-graph/graph.db`）。
- **技能**：`code-review-graph`（位置 `.kilo/skills/code-review-graph/SKILL.md`，kilo 会自动发现，无需改配置）。
  - 复杂任务 / 评审 / 重命名 / 架构体检前，先 `skill` 加载该技能，按其中已验证的调用姿势执行。
  - 技能内每个结论都在本仓库实测过（含全部已知坑与正确参数）。文件里已写好"评审流程 §4、搜索正确姿势 §5、坑速查 §7、日常节奏 §8"，直接照做。
- 每个任务**开头先** `get_minimal_context_tool(task="...")`，拿到风险分、相关 communities/flows、`head_matches_build`。

## 硬性纪律（违反会得到过期/错误结论）

1. **新鲜度**：`head_matches_build` 只反映 git HEAD commit，**不代表未提交改动已入图**。
   - 改过任何代码 / 有新的 commit 之后，先 `build_or_update_graph_tool()`，再用搜索/查询验证目标符号真的在。
   - 增量 build 的 summary（"No changes detected" / `files_updated:0`）**不可信**，以内容查询为准。
   - 避免 `full_rebuild=True`（❌ 会剔除**未跟踪文件**的节点；且可能 MCP 超时）。日常只用增量。
2. **评审未提交/未跟踪改动**：`detect_changes_tool(base="HEAD", changed_files=["src/a.rs", ...])` —— 必须配 `base="HEAD"`，否则会混入上次 commit 的结论。
3. **搜索**：`semantic_search_nodes` 用**单个、独特的标识符**（多词短语必 0 命中）；别用 `kind` 过滤（可能误杀）。想搜函数体 → 用 grep。
4. **结构查询**：`query_graph` 的 target 用**裸短名**（`evict_tiles`），或用上一次结果回传的 `qualified_name`（`F:/.../view.rs::PdfViewer.evict_tiles`）。点分名（`PdfViewer.evict_tiles`）会 not_found，常见名会 ambiguous——不要用。
5. **启发式输出要人工复核**：`refactor dead_code` 有假阳性（本仓库实测 `BracketPair` 被误判）；删除/改名前必须 grep 验证调用点。`get_impact_radius` 计数噪声大，只看相对大小。`detect_changes` 与 `get_review_context` 的 risk 可能不一致，以 detect_changes 为主。
6. **别传**：`get_flow_tool` / `get_community_tool` 不接受 `detail_level`（会 validation error）；flow_id 每次重建会变，用 `flow_name` 或先 `list_flows`。

## 多仓库依赖图（gpui zed）

- 已全局注册到 `~/.code-review-graph/registry.json`：`zed`（整个 zed checkout）、`gpui`（= zed 的 `crates/gpui`，用 `.code-review-graph` 标记目录单独注册为仓库根）。
- **已建图可查**：
  - `gpui`
  - `zed`
- 用法：
  - 跨仓搜：`cross_repo_search_tool("<裸短名>")`（单 token，多词短语必 0 命中）。
  - 单仓结构查询/搜索：`query_graph_tool` / `semantic_search_nodes_tool` 传 `repo_root`。
    - `gpui`：依赖 checkout 路径**运行时自动解析**，无需手填绝对路径/改盘符（rev 从 `Cargo.lock` 读，checkout 目录用 7 位短 rev）：
      ```powershell
      $base = Join-Path $env:USERPROFILE ".cargo\git\checkouts\zed-a70e2ad075855582"
      $rev  = [regex]::Match((Get-Content Cargo.lock -Raw), 'zed\?rev=([0-9a-f]{40})').Groups[1].Value.Substring(0, 7)
      $gpui = Join-Path (Join-Path $base $rev) "crates\gpui"   # 即 repo_root
      ```
      跨仓检索无需该路径：`cross_repo_search_tool` 会搜所有已注册仓库。
- **建图/状态日常走 MCP 工具**（`build_or_update_graph_tool` / `list_repos_tool` / `run_postprocess_tool` 等），**不需要 CLI 路径**；仅 register/命令行操作时用本机 venv（路径在 `.kilo/kilo.jsonc` 的 MCP 配置里，`.kilo` 不入库、无敏感信息外泄）。
- 新鲜度：依赖 checkout 是只读基线，一般无需重建；cargo 更新拿到新 checkout sha 后旧 checkout 路径失效——用上面动态解析命令会自动指向新 rev，必要时重新 `register` + `build`。

## 常规循环

- 探索/确认 → `query_graph` / `traverse_graph`（深度+token 预算）。
- 评审已提交改动 → `detect_changes(base="<ref>")` → `get_affected_flows` → `get_review_context(base, include_source=true, max_files=小, max_lines_per_file=小)`。
- 安全重命名 → `refactor_tool(mode="rename")` 预览 → `apply_refactor_tool(dry_run=true)` 审 diff → 应用。删除 → `dead_code` 结果过 grep 再说。
- 架构体检 → `get_knowledge_gaps` / `get_hub_nodes` / `get_architecture_overview` / `get_suggested_questions`。
- **落地改动与回归仍以 `read`/`grep` + `cargo test` 为准**：图负责"找得快、看得全"，不替代码精读与编译验证。