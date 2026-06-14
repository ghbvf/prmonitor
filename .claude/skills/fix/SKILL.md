---
name: fix
description: "问题诊断与修复：验证 + 根因 + 复杂度分级 + 修复方案 + 收尾。当用户说'这个问题存在吗''帮我分析这个 bug''诊断一下这个模块''修复这个问题'时触发。输入优先 PR 号（自动读 PR 评论），也支持 文件:行号 / 自然语言；多 findings 自动批量。issue 号 triage 走 `issues` 技能（建议 /ship 或 close）。"
argument-hint: "<#PR | 文件:行号 | 问题描述>"
allowed-tools: [Read, Write, Edit, Glob, Grep, Bash, Agent, AskUserQuestion]
---

# 问题诊断与修复

> 仓库 `ghbvf/prmonitor`，base 分支 `develop`；issue/PR/评论原子操作规范见 `issues`。复杂度分级（small/large）与严重度（P0–P3）语义见 `.claude/agents/reviewer.md` §评级。

---

## 输入解析

优先级：**PR 号**（裸数字先按 PR 试 → `gh pr view <N> --json reviews,comments` 读对话评论 + review 摘要（每条带 body/id/url/createdAt）；再 `gh api repos/ghbvf/prmonitor/pulls/<N>/comments --jq length` 探 inline review comments，>0 则 `gh api .../pulls/<N>/comments` 一并读入，=0 跳过；**只取最新一轮**——按 createdAt 倒序，跳过自己上一轮的 `pm:ship`/`pm:fix` 留痕（已处理），取最近一批 **review findings**：`pm:pr-review` 无损详表或人/codex 的 review/comment）> **文件:行号** > **自然语言**（Grep/Glob）。**issue 号 triage 走 `issues`**（判定后建议 `/ship #<N>` 或 file:line）。

---

## 阶段 1：问题定位

### 1.1 找到问题代码
按精度递进：明确路径 → Read；模糊描述 → Grep 类型/方法签名 → Grep 错误/注释 → Agent(Explore) 调用图。三层均无果 → AskUserQuestion。

### 1.2 追踪调用链 + 数据流
从问题代码向上（调用方）和向下（被调用方）追踪。跨 3+ 模块用 Agent(Explore)。同时追踪数据流：数据源 → 变换 → 消费者。

### 1.3 确认问题是否存在

| 状态 | 含义 | 下一步 |
|------|------|--------|
| **CONFIRMED** | 问题真实存在，可复现 | → 进入阶段 2 |
| **RESOLVED** | 已被修复（给证据：哪行 / 哪 PR） | → 向用户报告，结束 |
| **CHANGED** | 代码重构过，问题形态变化 | → 描述新形态，确认是否继续 |
| **CANNOT_VERIFY** | 无法确认（缺上下文 / 需运行时验证） | → AskUserQuestion |

输出含：状态 / 位置 / 调用链 / 数据流 / 问题描述（自己总结）。

### 1.4 复现测试（Reproduction Test First）

CONFIRMED 后、修复前，先构造能**复现问题**的测试用例：

1. 基于调用链和数据流，编写最小测试触发问题
2. 运行确认 FAIL（证明可复现）
3. 作为修复验收标准

| 场景 | 操作 |
|------|------|
| 已有测试可稍改复现 | 修改已有测试 + 确认 FAIL |
| 需新写测试 | Rust 在对应模块加 `#[cfg(test)]` 的 `fn xxx_bug_repro()` |
| 并发问题 | 写可触发竞态的测试 |
| 无法在单测复现（需运行时 / UI 状态） | 标注 `RUNTIME_ONLY`，跳过此步 |

```bash
cargo test --manifest-path src-tauri/Cargo.toml <test-name>   # 确认 FAIL
```

---

## 阶段 2：根因分析 + 复杂度分级（CONFIRMED 后执行）

### 2.1 根因三维度
- **代码层面**：哪行代码、哪个设计决策导致
- **架构层面**：是否系统性（Grep 同模式，1 处=局部，3+ 架构缺陷）。架构缺陷 → AskUserQuestion 确认局部修还是系统性重构
- **历史层面**：git log 搜同类已有修复，发现团队惯例，避免退化

### 2.2 影响范围
直接影响 / 间接影响（列受影响文件）/ 同类问题（Grep 相同模式数量）

### 2.3 复杂度分级（small / large）

判定依据（按顺序检查）：
1. 修复涉及多少文件？（`Grep` 搜所有受影响调用点）
2. 是否需改 trait 签名（`PrSource` / `ReviewEngine`）或跨切片契约（`model.rs` ↔ `types.ts`）？
3. 是否改并发 / codex 进程生命周期 / session 状态机语义？
4. 同类问题在其他切片是否重复？（1 处=局部，3+ 系统性）

**small** = 单文件/局部 ≤3 处 + 不改 trait/契约/并发语义 → 可自动修；**large** = 跨切片 / 改 trait 或 model 契约 / 改并发或进程语义 → 需人工决策。

> small 表示"容易修"，不代表"不重要"。IN_SCOPE/OUT_OF_SCOPE 由文件归属（2.4）决定，与复杂度无关——small 也可以是 IN_SCOPE 且必须修。

### 2.4 当前分支归属判定

1. `git diff --name-only origin/develop...HEAD` — 获取当前分支改动文件列表
2. 对比 finding 涉及文件是否在列表中
3. 当前分支有关联 PR（`gh pr view --json title,body`）→ 检查 PR 描述是否含该 finding ID/关键词

| 结果 | 判定条件（按文件归属快速判） | 下一步 |
|------|---------|--------|
| **IN_SCOPE** | finding 文件在当前分支 diff 中，或 PR 描述含该 finding ID | 在当前分支修复 |
| **RELATED** | 不在 diff 中但同切片 / 同子系统遗留 | 建议搭车修，标注"搭车" |
| **OUT_OF_SCOPE** | 完全不同的切片 / 模块 | 不在当前分支修；建议建 issue（4.6 step 3） |

输出含：三维根因、复杂度、归属（含理由）、影响范围（直接/间接/同类）、历史修复。

---

## 阶段 3：修复方案设计

### 方案设计原则（贯穿；进入阶段 4 / 输出 large 方案 / 提交批量汇总前自检）
- **彻底**：根因级修复，不留 TODO/FIXME/follow-up；2.2 列出的"同类"一并纳入
- **不向后兼容**：直接改签名/删字段/换实现，不留 deprecation 别名、shim、双路径
- **优雅简洁**：最少代码、抽象、新文件，不预设未来需求

不通过 → 修订；必须保留的违反项 → 显式列入"遗留 / 取舍说明"，不得默默放行。默认走彻底方案。

### 3.0 对标参考查询（large 必须执行）
large 问题先查参考再动手：Rust 标准库 / `tauri`·`serde`·`tokio` 等官方库推荐模式 + 已知陷阱 / Rust·Vue 社区惯例。
**何时跳过**：small 全跳过；纯业务 bug 跳过。
**不可跳过**（即使 small）：并发/锁、进程/连接生命周期、重连/重试/超时、认证/密钥、流式事件发布消费。

### 3.1 方案分级

| 复杂度 | 方案数 | 形态 |
|--------|--------|------|
| small | 1 | 直接修，跳过比较 |
| large | 2-3 | A 最小修复 + B 彻底方案（+ C 重构，按影响面） |

每个方案须含：改动范围、原理、优缺点、遗留（仅最小修复）、预估改动量、参考来源（large 必填）。

### 3.2 时机判断
- **Q0 是否属于当前分支？** 取 2.4：OUT_OF_SCOPE → 建 issue（4.6 step 3），跳过 Q1-Q3；IN_SCOPE/RELATED → 继续（RELATED 改动量大可 defer）。
- **Q1 现在做还是后面做？** 现在做（安全/崩溃/阻塞他人/≤50 行）｜本迭代（有影响不紧急/50-200 行）｜下迭代（设计级/200+ 行/有前置）｜记录不做（理论风险/修复代价远大于收益）。
- **Q2 能不能现在做？** 检查 issue 依赖、活跃分支冲突、trait 消费方。
- **Q3 最小修复有效期？** 给彻底方案的建议时间窗口。

### 3.3 详细修复计划
文件级改动清单 + 验证命令（`cargo build` / `cargo test` / `pnpm build`）。

### 3.4 执行决策（自动，不逐条问用户）

| 复杂度 | 条件 | 决策 |
|--------|------|-----|
| small + IN_SCOPE + ≤2 文件 + 不改 trait/契约/并发语义 | 全满足 | **[AUTO-FIX]** 直接修 |
| large + IN_SCOPE + 能做 | — | 执行推荐方案（A/B 比较） |
| large + 不能做（有前置依赖） | — | 记录报告，标注阻塞 |
| 任何 + OUT_OF_SCOPE | — | 不修，建议建 issue（4.6 step 3） |

**不可自动执行**：并发语义变更、trait 签名修改、跨切片契约（model.rs ↔ types.ts）变更、新依赖、数据流方向变更、large。

> **无监督路径**（被 review 自动驱动时）只跑 [AUTO-FIX] 一档；其余一律 surface + 转人工，绝不自动改。

### 3.5 执行前任务清单（阶段 3 → 4 门禁）
所有 finding 创建 task（OUT_OF_SCOPE 标 `[→ 建 issue]`）；单条 small IN_SCOPE → 跳过清单直接修；批量或 large → 必须创建。最后两项固定：`commit + push` + `收尾（评论 / issue）`。创建后立即执行，不等确认。

---

## 阶段 4：执行修复

### 4.1 Commit 格式
当前分支直接改。Commit：`fix(<scope>): <问题简述>` + 根因 + 复杂度 + Refs + `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`。scope 按切片：config/pr/codex/前端。安全约束：只 add 修复文件（不 add -A）；不 amend。

### 4.2 执行代码修改（逐编辑测试循环）

> **批量并行**：4+ 条 finding 时按切片（config/pr/codex/前端）聚类派发 `developer` sub-agent（**同切片同 agent** 防写冲突，组内串行执行下面循环）；并发 4-9→2 / ≥10→3；≤3 条由主 agent 直接处理。triage 同理可按聚类并行（`Explore`）。

对每个任务执行 Edit-Test Loop：
1. Read 目标文件 → 2. Edit/Write 修改代码 → 3. `cargo build --manifest-path src-tauri/Cargo.toml`（或 `pnpm build`）编译检查 → 4. `cargo test --manifest-path src-tauri/Cargo.toml`（含 1.4 复现测试）→ 5. 失败：当前编辑引入 → 立即修正重回 2；暴露后续依赖 → 记录继续 → 6. 通过 → 下一任务

### 4.3 最终测试
```bash
pnpm build                                                       # 前端类型检查 + build
cargo build  --manifest-path src-tauri/Cargo.toml --locked
cargo test   --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --locked -- -D warnings
cargo fmt    --manifest-path src-tauri/Cargo.toml --all -- --check
```

### 4.4 测试失败处理（分层回退）
Round 1-2 当前方案迭代修正｜Round 3 `git stash` + 切备选方案重新执行｜Round 4 回滚（`git checkout -- <文件>`），small 标 ESCALATE，large 降级最小修复标遗留。

### 4.5 验证修复
重跑阶段 1 定位逻辑，确认：原问题代码已替换 / 数据流已正确保护 / 测试覆盖了问题场景。

### 4.6 Git 收尾（测试通过后自动执行）

分四步：先提交代码，再冲突预检（阻塞），再立即收尾（评论），最后 CI 异步收敛。

**步骤 1 提交 + push**：`git add` 修复涉及的代码文件 → 按 4.1 commit → push；**PR 已存在则不重建**，仅当前分支尚无 PR 时才 `gh pr create --base develop`。

**步骤 2 冲突预检（阻塞，有 push 时；命令见 `issues` B4 ①）**：push 后验无文件冲突；通过后**立即**进步骤 3（不等 CI）。

**步骤 3 立即收尾（评论，不等 CI；命令见 `issues` B3）**：贴 `pm:fix` 评论（命令 + 回显 comment URL 见 `issues` B3；内联格式，无机器块）：

```markdown
<!-- pm:fix -->
## 🔁 fix（findings triage + fix）

**Findings** <总数>（已修 small <n> · 遗留 large <m> · OUT_OF_SCOPE <k>）

- **F1** [P1·small·安全] `path/to/file.rs:120` — <一句话> → ✅ 已修
- **F2** [P2·large·DX] `path/to/x.rs:88` — <一句话> → ⏸ 遗留（需人工决策）
- **F3** [P2·small·可靠性] `other/mod.rs:64` — <一句话> → 🚦 OUT_OF_SCOPE

<details><summary>完整详表（triage 依据 + 证据 + 建议，下次 fix 读此）</summary>

**F1** [P1·small·安全] `path/to/file.rs:120`（IN_SCOPE）
- 证据：`<code 片段>`
- 修复：<做了什么> → ✅ commit <sha>

**F2** [P2·large·DX] `path/to/x.rs:88`（IN_SCOPE，遗留）
- 三级方案种子：最小 <…> / 彻底 <…> / 重构 <…>
- 遗留原因 + 升级窗口：<…>
</details>

**下一步**：跑 `/pr-review #<N> --check` 验证修复（fix 不自证完成）。

---
🤖 PR #<N> · Generated with Claude Code · branch <head 分支> · worktree <路径|—> · session <会话id|—>
```

- **OUT_OF_SCOPE finding** → 建议建 backlog issue（`issues` B1，朴素 title+body，无四轴 label）：body 无损填 现状（证据+三维根因+影响）/ 修复方向（三级方案种子）/ Files（file:line 全集）/ Source（`PR #<N> finding <Fk>`，`Discovered via /fix #<N>`）。先反思确认确实 OUT_OF_SCOPE 且非 small 搭车修；incident/红线或归属不清 → 停下 AskUserQuestion，不静默自动建。
- **未修 large / RELATED deferred** → 输出 `gh issue create` 建议命令（确认后跑，留 open）。

**步骤 4 CI 异步收敛（非阻塞，步骤 3 完成后执行；有 push 时；命令见 `issues` B4 ②）**：等 CI 收敛 + 失败回阶段 1-4 修复循环再推再等（≤3 轮，3 轮仍红 → 停下交人工）；CI 收敛后在窗口报告结果（全绿 / 仍红 + 失败 check 摘要 + run 链接）。

---

## 阶段 5：输出 + 验证

窗口打印诊断 / 修复报告是主输出、pm:fix 评论（4.6 已贴）是无损留痕，两者都做。

- 诊断报告（未修）/ 修复报告（已修）/ 批量验证（审查报告）

**验证**（4.6 已执行，此处复核，不再查找）：核对 fix 评论已贴；OUT_OF_SCOPE → issue 已建议/已建；未修 large deferred → create 建议命令已输出待确认。

---

## 沟通规则

**默认按分析结果自动决策。** 仅以下情况用 AskUserQuestion：
- 无法定位问题代码
- 测试失败且 4 轮回退后仍无法修正
- 修复过程中发现新问题超出原始 scope
- OUT_OF_SCOPE finding 建 issue（先反思确认确实 OUT_OF_SCOPE 且非 small 搭车修，再无损填 body）；incident/红线或归属不清 → 停下确认
