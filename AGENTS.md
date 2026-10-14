# AGENTS.md

## 项目

Rust + GPUI 的编辑器/阅读器应用（`gpui-editor`）。P5 已拆 Cargo workspace：`src/main.rs` 只是薄二进制壳，功能全在各 crate（`crates/shell`=外壳/侧栏/EditorApp、`crates/editor`=编辑区、`crates/markdown`=Markdown 预览、`crates/pdf`=PDF 阅读、`crates/epub`、`crates/viewer`、`crates/image_viewer`、`crates/video`、`crates/syntax`、`crates/ui`、`crates/toc`、`crates/config`、`crates/base`、`crates/ime`）。

## 上下文保护与子 agent 流水线

**铁律**：主 agent 只做「派活 → 收报告 → 轻量核对 → 按用户指示提交/推送」；实现/评审/性能对比/问题定位一律子 agent（`task`：`general`=可动手、`explore`=只读）。主 agent **不读大文件、不精读大 diff、不亲手写功能代码**。例外：单文件小改动或依赖严格决策链的精确编辑，若委派明显更慢/更易失真才亲自动手。

### 一、任务包工（每次派活必写全）

1. **目标 + 已知落点**：`path:行号`（附"行号可漂移、先 grep/read 精读再动"，让子 agent 不从头摸）
2. **需求清单**：可勾选、逐条、可验收；凡**缓存/派生状态改动**，必须列全失效源（换文档/切预览/内容编辑/整篇换源/折叠/只读切换等）
3. **纪律**：不改 gpui checkout、不加依赖；不提交 git（提交权在主 agent，须用户明确说"提交git"）；颜色走 `ui::theme`、注释中文；先 `get_minimal_context_tool(task=...)`
4. **验证**：`cargo check --bin gpui-editor`（0 error/0 warning）+ `cargo test -p <crate>` 最小精确测试集 + 真实渲染路径回归——**边界清单逐项打勾**（非行 0、同行多元素、折行续行、文档结尾、空文档等；实现侧每项勾完才算完成，漏项留到评审=返工多耗一轮 FAIL→PASS 编译）
5. **报告**：改动 `path:line` → 机制 → 验证输出摘要 → 遗留风险 → 建议 commit message（几 KB 内，禁止转贴源码）

### 二、流水线

```
主 agent 轻侦察(grep 落点) → 实现子agent(brief) → 评审子agent(发现缺陷直接修)
→ 主 agent 轻核对(git status --short && git diff --stat) → 用户说"提交git"才提交
```

- 修 bug 也走同线：**复现测试先行**（真实渲染断言，先 FAIL 修到 PASS）；根因由实现子 agent 自证（引用代码证据）。brief 根因规则：**只写已核实的根因**（主 agent read 确证过才给，直接省实现时间）；**未核实的猜想必不写**，只给症状+入口点，避免引偏
- **实现→评审双 agent 只用于逻辑/几何/命中/并发类**；纯视觉/数值微调（k/曲率/间距等）自验即可；同文件连续微调用 `task_id` 续跑同一会话
- 评审分级：高风险共享模块（editor/view/core/shell/公共组件）亲自重跑；低风险单模块只读 diff + 抽查关键断言 + 定向跑，**与实现方共用同一份测试输出**
- 互不冲突的独立任务并行派（跨文件/跨模块）；同文件改动强制串行

### 三、编译与验证纪律（编译是耗时头号）

- **只跑 debug、禁止 release**；workspace 测试必须带 `-p <crate>`（根目录裸 `cargo test` 只盖根包+video-ffmpeg）；pdf 加 `--test-threads=1`
- **每组改动改完再编译**：`cargo test -p <crate> <filter>` 一次编译、复用产物连跑多组；`cargo check` 并入 `--tests` 或省略；实现侧压到 ~2 轮；大 crate（如 shell）一次改完再编、共享一次性能测量
- **验证只做改动部分、禁止全量**（全量仅当用户明确要求/特性定型）；行为功能**不跑 stash 基线**（回归由评审兜底）——仅性能对比子 agent 才 stash before/after，pop 后逐文件核对还原
- 边界测试先行 → 字节/几何盲区消灭在实现侧，评审不再补 FAIL→PASS

### 四、主 agent 三收敛（主上下文涨速主因）

- **侦察下放**：派活前只做 **1-2 条 CRG 查询**（`get_minimal_context_tool` / `query_graph` / `traverse_graph`）或 grep 定入口；**不 read 实现细节、不跑多路 grep/采样替子 agent 摸底**——子 agent 本就要读，主 agent 抢读既烧上下文又常常猜错根因
- **核对极简**：收报告后只跑一条 `git status --short && git diff --stat`；不再逐路复核（确需抽查至多 1 个 grep）
- **brief 收敛**：只写「需求 + 约束 + 已知落点」，不写实现规格；**根因只写已核实的、不写猜测**（见流水线条）；续跑轮只写 delta；歧义合并进一次 `question`；bench/大表输出一律 `-Last N`/limit 截断

## 既有验收口径（子 agent 验证口径，别当"回归"）

- 各 crate 单独验证：`cargo test -p <crate>` 全绿；测试数随迭代波动，别锁旧计数
- pdf 夹具 = 仓库根 `target/pdf测试.pdf` + `target/pdf测试2.pdf`（不入库、手动装配；crate 经 `CARGO_MANIFEST_DIR/../../target` 定位）。缺夹具打 `SKIP: missing target/pdf测试*.pdf` 后 return——**SKIP 是常态不是回归**
- 测试窗宽抖动；debug ≈ release 的 ~10×；看相对变化与 stable 子项，别看绝对值；数据处理用 python；rg 不一定在 PATH → 用 grep/glob

## 已踩坑速查

- 正文卡包装层必须是 flex 容器，否则 `flex_1` 高度坍缩毁滚动/命中
- `deferred(anchored(Window))` 定位用 `.position(窗口坐标)`，不用 `.offset(...)`（会顶到窗口底/沉底）
- 欢迎页/零 tab：`tabs` 允许为空，凡 `tabs[active_tab]` 消费一律 `tabs.get(...)` 兜底
- `with_spring`/`hover_spring`：临界阻尼免过冲；`reduce_motion` 由 gpui 自动尊重
- 大文档性能红线：不得引入持续逐帧重绘（弹簧收敛即静止）；状态栏等只读派生须 O(1)

## 代码探索与评审：优先用本机 CRG 工具链

- MCP `code-review-graph` 已启用（`.kilo/kilo.jsonc`，本地 venv，仓库根 `.code-review-graph/graph.db`）；**不要为查"谁调用谁/影响面"手工通读 2000 行整文件**
- **新鲜度靠 CLI 保鲜**：本机 `code-review-graph watch` 常驻自动增更（仓库根 `.code-review-graph/` 不入库）；图暂停过/换 checkout 后先 CLI `build` 拉新再查。**`get_minimal_context`/搜索报 stale 且排查对象不在近期改动里 → 直接用单 token 搜索 + grep 寻路，不追更**（`update` 报 0 常因增量基准未随 HEAD 推进；确需拉新用 `build`）
- **技能**：复杂任务/评审/重命名/架构体检前先 `skill` 加载 `code-review-graph`（`.kilo/skills/`，自动发现），按其已验证调用姿势执行（评审 §4、搜索 §5、坑 §7、节奏 §8）
- 每个任务开头先 `get_minimal_context_tool(task=...)`，拿风险分、相关 communities/flows、`head_matches_build`

## 硬性纪律（违反会得到过期/错误结论）

1. **新鲜度**：`head_matches_build` 只反映 git HEAD，不代表未提交改动已入图——改码/commit 后先 `build_or_update_graph_tool()` 再验证目标符号；增量 summary（"No changes detected"）不可信，以内容查询为准；避免 `full_rebuild=True`（剔除未跟踪文件节点）
2. **评审未提交改动**：`detect_changes_tool(base="HEAD", changed_files=[...])`，必须配 `base="HEAD"`
3. **搜索**：`semantic_search_nodes` 用单个独特标识符（多词 0 命中）；别用 `kind` 过滤（误杀）；搜函数体用 grep
4. **结构查询**：`query_graph` 的 target 用裸短名或 qualified_name；点分名 not_found
5. **启发式人工复核**：`dead_code` 有假阳性（`BracketPair` 被误判）；删除/改名前 grep 验证调用点
6. **别传**：`get_flow_tool`/`get_community_tool` 不接受 `detail_level`；flow 用 name 查（id 每次重建会变）

## 多仓库依赖图（gpui zed）

- 已注册进 `~/.code-review-graph/registry.json` 并**已建图可查**：`gpui`、`zed`（gpui = zed 的 `crates/gpui`）
- 跨仓检索：`cross_repo_search_tool("<裸短名>")` 直达所有注册仓库，**无需路径**
- 单仓查询：`query_graph_tool` / `semantic_search_nodes_tool` 传 `repo_root`——gpui checkout 路径**运行时自动解析**，无需手填/改盘符（rev 从 `Cargo.lock` 读、checkout 目录用 7 位短 rev）：
  ```powershell
  $base = Join-Path $env:USERPROFILE ".cargo\git\checkouts\zed-a70e2ad075855582"
  $rev  = [regex]::Match((Get-Content Cargo.lock -Raw), 'zed\?rev=([0-9a-f]{40})').Groups[1].Value.Substring(0, 7)
  $gpui = Join-Path (Join-Path $base $rev) "crates\gpui"   # 即 repo_root
  ```
- 建图/状态日常走 MCP 工具（`build_or_update_graph_tool`/`list_repos_tool` 等），无需 CLI 路径；仅 register/CLI 用本机 venv（在 `.kilo/kilo.jsonc`，不入库）
- cargo 更新拿到新 checkout sha 后旧路径失效：上面命令自动指向新 rev，必要时重新 `register` + `build`

## 常规循环

- 探索/确认 → `query_graph` / `traverse_graph`（带深度 + token 预算）
- 评审已提交改动 → `detect_changes(base="<ref>")` → `get_affected_flows` → `get_review_context(base, include_source=true, max_* 取小)`
- 安全重命名 → `refactor_tool(mode="rename")` 预览 → `apply_refactor_tool(dry_run=true)` 审 diff → 应用；删除 → `dead_code` 结果过 grep 再说
- 架构体检 → `get_knowledge_gaps` / `get_hub_nodes` / `get_architecture_overview` / `get_suggested_questions`
- **落地改动与回归仍以 `read`/`grep` + `cargo test` 为准**：图负责找得快、看得全，不替代码精读与编译验证
