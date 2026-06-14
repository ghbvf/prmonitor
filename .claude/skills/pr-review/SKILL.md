---
name: pr-review
description: "对指定 PR 跑自动分级六维度 review（默认）；或 --check 模式验证上一轮 findings 是否修复 + 抓回归。按 diff 净增删行数自动分配 2/3/6 reviewer agent 并行（< 200 行不派发，主 agent 自审）；主 agent 做根因聚类 + 复杂度分级 + 修复分流建议，不自动 fix。"
argument-hint: "<PR 编号> [--check]"
allowed-tools: [Read, Glob, Grep, Bash, Agent]
disable-model-invocation: true
---

# prmonitor PR Review — 自动分级六维度审查

按 PR diff 净增删行数自动分配 2/3/6 个 `reviewer` agent 并行做六维度审查（< 200 行不派发，主 agent 自审），主 agent 做根因聚类与修复分流建议。**只 review，不自动 fix**。仓库 `ghbvf/prmonitor`。

---

## 阶段 1：输入解析

参数：`<PR 编号> [--check]`。剥离 `--check` flag 后，剩余必须是 PR 编号（纯数字或 `#NNN`）。

- 缺参 → 输出 `错误：缺少 PR 编号；用法：/pr-review <PR 编号> [--check]`，不执行后续
- 非法格式 → 输出 `错误：参数 "<原值>" 不是合法 PR 编号`，不执行后续
- **带 `--check`** → 走下方 **模式 B：--check 验证**，跑完即返回
- **不带** → 走默认全新六维 review（阶段 2-6）

---

## 阶段 2：取 diff 行数

```bash
gh pr view <N> --json additions,deletions --jq '.additions + .deletions'
```

取不到 PR → 报错退出。

---

## 阶段 2.5：定位或自动创建 review worktree

```bash
BRANCH=$(gh pr view <N> --json headRefName --jq .headRefName)
git worktree list   # 找 [<BRANCH>] 所在行，其首列路径即该分支 worktree，记为 WORKTREE；无匹配 → 情况 B
```

**情况 A：找到既有 worktree** → 直接用 `$WORKTREE`，不动其状态。

**情况 B：无既有 worktree** → 自动创建 review-only worktree：

```bash
git fetch origin <BRANCH>
git worktree add --detach worktrees/review-pr<N> origin/<BRANCH>   # 已存在则 git -C ... reset --hard origin/<BRANCH> 刷新
WORKTREE="$(git rev-parse --show-toplevel)/worktrees/review-pr<N>"
```

创建失败 → 报错退出，不静默回退。情况 B 在阶段 6 末尾追加：`🧹 清理：git worktree remove worktrees/review-pr<N>`。

---

## 阶段 3：PR 元数据上下文

```bash
gh pr view <N> --json title,body,files,headRefOid
```

以下读取全部以 `$WORKTREE` 为根；`gh pr view.files` 只提供 repo-relative path，不作为文件内容来源。若仓库根有 `CLAUDE.md` 则读取确认约束。架构约束 / 六维度 / 评级沿用 `.claude/agents/reviewer.md`。

---

## 阶段 3.5：分级表

**派发档位（reviewer 数 + 维度切分）单源 = `.claude/agents/reviewer.md` §派发分档**，按阶段 2 的 diff 行数定档（区间左闭右开，边界归更高档）。

`diff < 200`：不派发 sub-agent，主 agent 在阶段 3 的上下文内按 reviewer.md（六维度 / Finding 格式 / 评级）在 `$WORKTREE` 上 Read/Grep 自审，直接进入阶段 5。`diff ≥ 200` 按 §派发分档 派 2/3/6 个 reviewer。

---

## 阶段 4：并行派发 reviewer agent

> `diff < 200` 跳过本阶段，直接执行阶段 5。

单消息内多 `Agent` tool call 并行启动 `subagent_type: reviewer`。每个 sub-agent prompt 必须自包含：

- PR 编号 + 取 diff 命令 `gh pr diff <N>` / `gh pr view <N> --json title,body,files,headRefOid`
- 工作目录 `$WORKTREE` 绝对路径，所有 Read/Grep 路径前缀 `$WORKTREE/`
- Finding 的 `文件:行号` 必须是 **repo-relative**（去掉 `$WORKTREE/` 与 `worktrees/<name>/` 前缀）
- 分配的维度子集（见 reviewer.md §派发分档）
- Finding 格式、复杂度分级、输出契约 → 沿用 `.claude/agents/reviewer.md`

---

## 阶段 5：主 agent 分析与汇总

收齐 Finding 后（`diff ≥ 200` 来自 sub-agent；`diff < 200` 来自主 agent 自审），主 agent **必须自己读关键文件、做根因分析**，不允许原样转发。

### 5.1 去重 / 冲突裁决

- 同 `文件:行号` + 同描述 → 重复，保留更详细一条
- 同 `文件:行号` 不同 P 级 → 保留更高 P 级，Read 代码裁定是否降级
- 同 `文件:行号` 不同复杂度 → 按 5.2 整簇重评
- 冲突结论（P0 vs LGTM）→ Read 代码亲自裁决

### 5.2 根因归类（不允许跳过）

按**共同根因**聚类（不按文件 / 不按维度）。`Grep` 验证系统性：≥ 3 处 = 架构/系统性缺陷（多半 large），1-2 处 = 局部 bug（small）。每簇标注：涉及维度 / Finding 数（按 P 级拆分）/ 系统性（Grep 数）/ 整簇复杂度。

### 5.3 输出 5 块（**先打印到对话窗口给用户**，顺序固定）

**输出语言**：中文。这 5 块是主交付物——必须先在对话/窗口完整打印给用户看，阶段 6 再贴成 PR 评论留痕（窗口=主输出、评论=留痕，两者都做）。

1. **根因簇视图**（主输出）— 每簇：根因一句 / 维度 / Finding 数 / 系统性 / 整簇复杂度 / 子 Finding ID / 修复顺序建议
2. **Finding 详表** — list 形式（不用表格，避免 CJK 竖排）。按 P0→P3、同级 small→large 排序，每条两行：
   - `**F{n}** [P·复杂度·维度] repo-relative-path:line → 簇 C{m}`
   - 缩进 2 空格的摘要 ≤ 60 字，纯文本
3. **复杂度汇总** — 按根因簇 + 按 Finding 两套：`small: N / large: N`
4. **修复分流** — small 簇 → `/fix`；large 簇 → "需人工决策" + 三级方案种子（最小/彻底/重构）。若 PR body（阶段 3 已取）含 GitHub closing keyword（`close[sd]?` / `fix(e[sd])?` / `resolve[sd]?` / `refs`，大小写不敏感）后跟 `#<N>`，分流条目附 `← issue #<N>`
5. **总体结论** — `通过 / 需修复 / 需讨论` + 一句话理由

输出前自检：① 每个根因簇都 Read 过代表文件？② 根因到了根本层（不停在症状）？③ 系统性判定有 Grep 证据？— 任一不通过 → 补做。

---

## 阶段 6：贴 PR 评论（留痕，**不替代阶段 5 的窗口打印**）

阶段 5 五块**已打印到窗口后**，把**同一份内容**写进 `pm:pr-review` 评论（内联格式，无机器块），贴到 PR（命令 + 回显 comment URL 见 `issues` B3）：

```markdown
<!-- pm:pr-review -->
## 🔍 pr-review（六维度分级审查）

**根因簇** <N> · **Findings** <M>（P0 <a>·P1 <b>·P2 <c>·P3 <d> ｜ small <x>·large <y>）· **结论** <通过/需修复/需讨论>

**根因簇**
- **C1** <根因一句>（维度 <…>；系统性 Grep <N> 处）→ F1,F3

**Findings**（每条带 file:line，/fix 无损提取）
- **F1** [P1·small·安全] `path/to/file.rs:120` — <一句话> → 簇 C1
- **F2** [P2·large·DX] `path/to/x.rs:88` — <一句话> → 簇 C1

<details><summary>完整详表（证据 + 建议 + 根因 + 方案种子，/fix 读此）</summary>

**F1** [P1·small·安全] `path/to/file.rs:120`（→ C1）
- 证据：`<code 片段>`
- 建议：<彻底修复方向>

**F2** [P2·large·DX] `path/to/x.rs:88`（→ C1）
- 证据：`<code 片段>`
- 三级方案种子：最小 <…> / 彻底 <…> / 重构 <…>
</details>

**修复分流**：small → `/fix`；large → 需人工决策（方案种子见详表）。<若 PR body 含 closing keyword 附 `← issue #<N>`>
**结论**：<一句话理由>

---
🤖 PR #<N> · Generated with Claude Code · branch <head 分支> · worktree <路径|—> · session <会话id|—>
```

贴失败（非 0 退出）则报错退出，不静默跳过。**按结论切 label**（命令见 `issues` B5）：有 findings → `pr-review/changes-requested` + `pr-status/needs-fix`，窗口提示「下一步 `/fix #<N>`」；无 findings → `pr-review/approved` + `pr-status/ready`（终态），提示可合并。**不自动启动监控**（流转由人工触发的 review/fix/check 各步切 label，无后台轮询）。情况 B 创建的 worktree 在此提示清理。

---

## 模式 B：--check 验证（确认上一轮 findings 是否修复 + 抓回归）

> `/pr-review <PR#> --check`：**不做全新六维 review**，只验证上一轮发现的问题是否真修复，并在这些站点抓 `/fix` 引入的回归。

### B1 读上一轮 findings（无损源）

**优先当前会话窗口**：同 session 内刚跑过 `/pr-review` 或 `/fix`、findings 已在上下文 → 直接用，不重复拉取。窗口没有 → `gh pr view <N> --json comments,reviews`，按 createdAt 倒序锁定**最近一次 review findings**（`pm:pr-review` 的 `<details>` 无损详表，或人/codex 的 review/comment，每条带 `file:line` + 证据 + 建议）+ 其后 `pm:fix`（fix 声称修了什么）。两者都无 → 报错退出（无可验证项）。

### B2 定位 worktree

同阶段 2.5（复用既有 worktree 或自动建 review-only worktree，读当前 head 代码）。

### B3 逐条验证（Read 当前代码，只信代码）

| 状态 | 判据 |
|------|------|
| ✅ 已修复 | 原问题代码已按建议改掉，证据充分 |
| ❌ 未修复 | 原问题代码仍在（pm:fix 声称修了但实际没改） |
| ⚠️ 回归 | 修了原问题，但在该站点 / 调用链引入新问题（`Grep` 调用方确认） |
| 🔧 部分 | 只修一部分 / 留了 TODO |

**每条必须 Read 实证，不凭 pm:fix 的"已修"自述**（review 只信代码）。

### B4 输出（窗口=主输出）

1. **验证表**（主输出，逐条）：`F{n} [原 P·复杂度·维度] repo-relative-path:line → ✅/❌/⚠️/🔧 + 一句证据`
2. **汇总**：已修复 N / 未修复 M / 回归 K / 部分 J
3. **结论 + 建议**：
   - 全 ✅ → 可合并
   - 有 ❌/⚠️/🔧 → 未修/回归项带 `file:line` 回 `/fix #<N>`

### B5 贴 pm:pr-review（--check 留痕）

窗口打印 B4 后，贴 `pm:pr-review` 评论（--check 变体：每条 finding 带 ✅/❌/⚠️/🔧 状态替代簇归属，summary 用 已修复N/未修复M/回归K；详表 `<details>` 记每条验证证据）。命令 + 回显见 `issues` B3。**按验证结果切 label**（命令见 `issues` B5）：全 ✅ → `pr-status/ready` + `pr-review/approved`（终态），提示可合并；有 ❌/⚠️/🔧 → `pr-review/changes-requested` + `pr-status/needs-fix`，窗口提示 `/fix #<N>`（回修复循环）。**不自动启动监控**。

---

## 约束

- 默认模式不调用 `/fix`，不写代码，不评 CI（贴 pm:pr-review 评论=留痕，不算改代码）
- `--check` 模式同样不写代码 / 不调 `/fix`；只读代码验证 + 贴评论
- review/--check 各步按结论切双轴 label（命令见 `issues` B5）；不启动后台自动监控（人工触发各步，无轮询循环）

---

## 验证清单

1. 缺参 / 非法参数 → 立即输出错误，不执行后续
2. 分级处理：`diff < 200` 主 agent 自审不派发；`diff ≥ 200` 按行数派 2/3/6 个 reviewer agent（覆盖三档）
3. 无 worktree 自动创建 `worktrees/review-pr<N>`；既有 worktree 复用，不重建
4. 主 agent 输出含 Read/Grep 证据 + 根因簇视图先于 Finding 详表；维度名内部一致
5. 阶段 6 贴 `pm:pr-review` 评论（含 footer + 每条 finding 的 file:line）+ 回显 comment URL/id
6. `--check` 模式：读上一轮 findings → 逐条 Read 验证 ✅/❌/⚠️/🔧（含抓回归）→ 窗口主输出验证表 + 贴 pm:pr-review（--check）→ 按结论切 label（全 ✅ → ready+approved；有遗留 → needs-fix+changes-requested，命令见 `issues` B5）
