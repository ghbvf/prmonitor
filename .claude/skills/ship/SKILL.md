---
name: ship
description: "全流程实施：探索→计划→worktree→实施→PR→内置 review→自动修小问题→人工确认。L1(跳过探索,1 reviewer)/L2(单 agent 探索,1 reviewer)/L3(默认,三 agent 探索,按 diff 行数 1/2/3/6 reviewer 自动)。"
argument-hint: "[--level=L1|L2|L3] <#issue-number 或任务描述>"
allowed-tools: [Read, Write, Edit, Glob, Grep, Bash, Agent, AskUserQuestion]
---

# prmonitor Ship — 全流程实施

> **多沟通原则（默认多问、有歧义即停）**：L2/L3 在创建 worktree（阶段 3）**之前**必须完整呈现「方案方向（阶段 1）+ 改动计划（阶段 2）」并经 AskUserQuestion 确认——不在未对齐时就开工。实施中（阶段 5）surface 阶段性进度与 blocker；阶段 7→8 呈现内置 review findings 表，**small + IN_SCOPE 自动修**，仅 large / 归属-取舍不清才停下问。任何方案歧义 / 范围不清 / 取舍没把握 → 停下问，不默默假设。

剥离 `--level=` flag 后，剩余参数匹配 `^#?[0-9]+$` 时视为 issue 号，先 `gh issue view <N> --json title,body,labels,state` 拉取作为任务上下文；后续阶段以 issue title/body 替代自由文本任务描述，阶段 6 PR body 追加 `Closes #<N>`。`state != "OPEN"` 或 `gh issue view` 失败均用 AskUserQuestion 让用户裁定是否继续。

## 等级

| 等级 | 探索 | 计划确认 | 实施 agent | review |
|------|------|---------|-----------|--------|
| L1 | 不探索 | 不需要 | 1-2 并行 | 1 reviewer |
| L2 | 1 explorer | 展示给用户 | 1-2 并行 | 1 reviewer |
| L3（默认） | 3 并行 explorer | AskUserQuestion 确认 | ≤ 4 并行 | 1/2/3/6 reviewer（按 diff 行数自动，见阶段 7） |

> **Agent 类型映射**（本仓 `.claude/agents/` 只内置 `reviewer.md` 一个自定义 agent）：探索（explorer 角色）→ `subagent_type: Explore`（只读检索）；实施（developer 角色）→ `subagent_type: general-purpose`（可编辑 + 跑测试）；审查 → `subagent_type: reviewer`（本仓 `.claude/agents/reviewer.md`）。下文沿用 explorer/developer/reviewer 角色名，dispatch 时按此映射取内置或本仓 agent。

---

## 阶段 1：探索（L1 跳过）

**L2**：启动 1 个 `explorer` agent，研究本仓既有实现模式与对标做法（Tauri command / serde 契约 / 切片 trait seam），提取接口签名、生命周期、错误处理关键设计，输出采纳建议和偏离理由。

**L3（默认）**：并行启动 3 个 `explorer` agent：
1. **本仓既有模式 + 对标参考实现**（切片结构、trait seam、Tauri/serde 用法、Rust/Vue 社区惯例）
2. 测试策略（cargo test table-driven / 集成 / serde 往返 覆盖模式）
3. 边界条件与安全处理（子进程调用、外部输入、进程生命周期）

全部完成后按「方案与计划原则（含自检）」汇总并逐条自查，再**用 AskUserQuestion 与用户确认方案方向**后继续。

---

### 方案与计划原则（含进入下一步前的自检）

阶段 1 汇总 / 阶段 2 计划必须满足下列原则；L3 在 AskUserQuestion 前逐条自查，任一不通过 → 在确认问题中**显式列出取舍及理由**，不默认放行：

- **彻底**：根因 + 完整解法，范围内紧密相关的小工作一并纳入。自查「是否还藏 TODO/FIXME/follow-up、兼容代码、未列入范围的关联工作？」→ 合并进当前 PR 或写明 blocker 理由。
- **不向后兼容**：删字段/改签名/换实现直接做。自查「是否留了 deprecation 别名、旧字段、兼容 shim、双路径？」→ 删掉或写明保留理由。
- **优雅简洁**：最少代码改动达成目标，不引入新抽象层、不预设未来需求。自查「能否用更少的代码/抽象/新文件达成同样目标？」→ 简化或写明保留理由。
- **对标参考**：做了嘛，方向正确吗（对标 Tauri / Rust / Vue 社区惯例与本仓既有切片）。

---

## 阶段 2：计划

按「方案与计划原则（含自检）」生成改动文件清单（按依赖顺序）、任务分组（串行/并行批次）、先写测试清单（有可测逻辑时）、对标参考（`ref: 文件`）。生成后逐条自查，L3 用 AskUserQuestion 与用户确认计划后继续。

**并行批次分析**（改动文件 ≥ 4 时必须在计划中明确）：
- 标注各任务的文件归属和批次编号
- 标注批次间依赖关系（有依赖 → 串行；无依赖 → 可并行）
- 解决同文件冲突：同一文件必须归入同一批次/agent

---

## 阶段 3：Worktree

worktree 约定（内联，无独立 git-worktree 技能）：

```bash
git fetch origin
git worktree add worktrees/<Type>/<issue#-short-name> -b <Type>/<issue#-short-name> origin/develop
```

约定：
- 目录 `worktrees/<Type>/<issue#-short-name>`（有关联 issue）/ `worktrees/<Type>/<short-name>`（无 issue）；分支名镜像该 path
- **编号 = 关联 issue 编号**（无关联 issue → 不编号）；**Type**（path 首段 + 分支首段）按关键字判定：Feature（默认）/ Fix（fix,bug,hotfix）/ Refactor（refactor,cleanup,rename）/ Docs（docs,adr）/ Experiment（poc,spike）
- 基准 `origin/develop`，创建前 `git fetch origin`
- **禁止 `cd worktrees/xxx`**；替代：`git -C worktrees/<wt> ...`、`cargo ... --manifest-path worktrees/<wt>/src-tauri/Cargo.toml`、`pnpm -C worktrees/<wt> ...`
- 用完即删（合并后，见阶段 9）。下文 `worktrees/<wt>` 简写指该 worktree 目录。

---

## 阶段 4：先写测试（有可测逻辑时）

在 worktree 中，对有可测逻辑的改动**先写测试**（Rust `#[cfg(test)]` / `*_test` 模块覆盖正常/边界/错误路径），运行确认测试先 **FAIL**，再进入实施：

```bash
cargo test --manifest-path worktrees/<wt>/src-tauri/Cargo.toml --locked
```

前端暂无测试运行器 → 类型检查（`pnpm -C worktrees/<wt> build` 的 vue-tsc 步）是前端门。纯配置/纯 UI/纯文档改动无可测逻辑时跳过本阶段并注明。

---

## 阶段 5：实施

### 5.0 分组与并行度决策（实施前必须执行）

主 agent 根据阶段 2 的改动文件清单和批次依赖关系，**自主决定**：
- 哪些任务无文件交叉且无逻辑依赖 → 可并行启动 developer agent
- 哪些任务有依赖或改同一文件 → 串行或归入同一 agent

**硬约束**：
- 同一文件只能分给同一 agent（防写冲突）
- 有前置依赖的批次必须等上一批全部完成后再启动
- 并行 developer agent 上限 **4 个**

### 5.1 Sub-agent prompt 自包含要求

每个 developer sub-agent prompt 必须包含：
- worktree 路径（`worktrees/<wt>`）
- 分配的任务列表（文件路径 + 改动描述）
- 命令格式：`cargo build/test --manifest-path worktrees/<wt>/src-tauri/Cargo.toml --locked`、`pnpm -C worktrees/<wt> build`
- 架构约束（切片自包含、跨切片只走 `model.rs`、trait seam `PrSource`/`ReviewEngine`、组装根 `lib.rs`、前后端类型对齐 `types.ts`↔`model.rs`）
- commit 格式：`<type>(<scope>): <描述>` + `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`

每个 sub-agent 在自己负责的任务上**串行**执行 Edit-Test Loop，完成后跑 lint（0 issues 才 commit）：

```bash
cargo fmt    --manifest-path worktrees/<wt>/src-tauri/Cargo.toml --all -- --check
cargo clippy --manifest-path worktrees/<wt>/src-tauri/Cargo.toml --all-targets --locked -- -D warnings
```

### 5.2 主 agent 汇总（所有并行 agent 完成后）

```bash
pnpm -C worktrees/<wt> build                                                       # vue-tsc + vite build
cargo build  --manifest-path worktrees/<wt>/src-tauri/Cargo.toml --locked
cargo test   --manifest-path worktrees/<wt>/src-tauri/Cargo.toml --locked
cargo fmt    --manifest-path worktrees/<wt>/src-tauri/Cargo.toml --all -- --check
cargo clippy --manifest-path worktrees/<wt>/src-tauri/Cargo.toml --all-targets --locked -- -D warnings   # 0 告警才进阶段 6
```

---

## 阶段 6：PR

```bash
git -C worktrees/<wt> push -u origin <branch>
gh pr create --base develop --title "..." --body-file <填好的 .github/pull_request_template.md>
```

PR body 结构单源 = `.github/pull_request_template.md`；读模版填占位（`Refs: Closes #<ID>` + `ref: 文件`），不在技能内重述结构。本仓 PR 全程 CLI 创建，必须 `--body-file` 读填好的模版。**创建后切 `pr-status/in-progress`**（命令见 `issues` B5）。

---

## 阶段 7：Review（内置 reviewer）

> ship 的 review 是内置首审（六维 reviewer）；外部再审走 `/pr-review <PR#>`，续修走 `/fix <PR#>`（人工驱动触发，review→fix→check 走双轴 label 流转，命令见 `issues` B5）。

**L1/L2**：1 个 `reviewer` agent（六维度）。

**L3**：按 PR diff 净增删行数确定 `reviewer` agent 数量：

```bash
git -C worktrees/<wt> diff --shortstat origin/develop   # N files changed, X insertions(+), Y deletions(-)
```

diff 行数 = X + Y（缺项按 0 计）。**reviewer 数 + 维度切分单源 = `.claude/agents/reviewer.md` §派发分档**（按算出的 diff 行数定档，区间左闭右开，边界归更高档）。

多 agent 时并行启动，每个 agent prompt 自包含其负责维度 + worktree 绝对路径（Read/Grep 前缀 `worktrees/<wt>/`，Finding 的 `file:line` 去前缀转 repo-relative）；Finding 格式 / 评级沿用 `.claude/agents/reviewer.md`。全部完成后由主 agent 汇总去重 findings 表（含 small/large 分级）。

---

## 阶段 8：Fix（内置审 findings）+ 收尾

1. **先在对话窗口完整打印内置 review findings 表**（主输出：含 P/复杂度分级 + IN_SCOPE/OUT 归属 + 每条 `file:line`），再贴 pm:ship 评论留痕——窗口=主输出、评论=无损留痕，两者都做。
2. **small + IN_SCOPE findings 自动修**（派 `developer` agent 直接 Edit-Test 修，**不逐条问**）；large 遗留与 OUT_OF_SCOPE 不自动改。**仅当**归属不清 / 取舍没把握 / 有 large 需现在做时才 AskUserQuestion。
3. **推送 + 冲突预检（阻塞）**：`git -C worktrees/<wt> push` 推送内置修复 commits；按 `issues` B4 ① 先验无文件冲突（冲突则 `merge origin/develop --no-edit` 解冲突再 push）。冲突预检通过后**立即**贴评论（不等 CI）。
4. **贴 pm:ship 评论**（命令 + 回显 comment URL 见 `issues` B3）。评论格式（内联，无机器块）：

   ```markdown
   <!-- pm:ship -->
   ## 🛠 ship review + fix

   **reviewer** <数> · **Findings** <总数>（已修 small <n> · 遗留 large <m> · OUT_OF_SCOPE <k>）

   - **F1** [P1·small·安全] `path/to/file.rs:120` — <一句话> → ✅ 已修
   - **F2** [P2·large·DX] `path/to/x.rs:88` — <一句话> → ⏸ 遗留（需人工决策）
   - **F3** [P2·small·可靠性] `other/mod.rs:64` — <一句话> → 🚦 OUT_OF_SCOPE

   <details><summary>完整详表（根因 + 证据 + 建议 + 方案种子，/fix 读此）</summary>

   **F1** [P1·small·安全] `path/to/file.rs:120`
   - 证据：`<code 片段>`
   - 建议：<彻底修复方向>
   - 处置：✅ 已修（commit <sha>）

   **F2** [P2·large·DX] `path/to/x.rs:88`
   - 证据：`<code 片段>`
   - 三级方案种子：最小 <…> / 彻底 <…> / 重构 <…>
   - 处置：⏸ 遗留（原因：<…>）
   </details>

   **下一步**：切 `pr-status/needs-review-again`（待再审：codex / `/pr-review #<N>`；有需改再 `/fix #<N>`）。

   ---
   🤖 PR #<N> · Generated with Claude Code · branch <head 分支> · worktree <路径|—> · session <会话id|—>
   ```

   > 评论是 `/fix` 与再审提取 findings 的来源——每条 Finding **必带 `file:line`**，根因 + 证据 + 建议 + 三级方案种子写进 `<details>`（人看摘要、fix 读详表）。OUT_OF_SCOPE finding 主列表给一行指针，详情贴独立 `pm:oos` 评论（模板见 `issues` B3；建单走 `issues` B1，朴素 title+body，无四轴 label；incident/红线/归属不清 → 停下 AskUserQuestion）。

5. **贴完切 label**：`pr-status/in-progress → needs-review-again`（命令见 `issues` B5；首审唯一使用点）。

6. **CI 异步收敛（非阻塞，切 label 后执行）**：按 `issues` B4 ② 等 CI 收敛 + 失败回修复循环再推再等（≤3 轮，3 轮仍红 → 停下交人工）；CI 收敛后**贴独立 `pm:ci` 评论**（模板见 `issues` B3）并在窗口报告结果（全绿 / 仍红 + 失败 check 摘要 + run 链接）。

> ship 到此结束（内置审 + 修；评论 + 切 label + CI 异步收敛）。外部再审走 `/pr-review #<N>`，续修走 `/fix #<N>`（人工触发，review→fix→check 走双轴 label 流转，命令见 `issues` B5）。

---

## 阶段 9：人工确认

```
PR: #<编号> <URL>
评论: <pm:ship 评论 URL，含 #issuecomment-<id>>
已完成：实施 / PR / review（实跑 reviewer 数：按 diff 1/2/3/6 自动） / small fix / CI 绿

未处理问题（需人工确认）——摘要 + 指针；完整无损详表见 pm:ship 评论的 `<details>`：
| # | Finding (file:line) | 复杂度 | 归属 | 建议方案 | 原因 |
|---|---------------------|------|------|---------|----|
```

合并后提示用户手动删除 worktree：**先回主仓库目录**，再 `git worktree remove worktrees/<wt>`（不自动删除；防工作目录丢失 / 会话异常）。

---

## 约束

- lint 0 issues 才 push；不 `--no-verify`；不 amend 已 push commit
- worktree 合并后提示用户手动 `git worktree remove`，删除前先退出 worktree 会话并回主仓库目录
