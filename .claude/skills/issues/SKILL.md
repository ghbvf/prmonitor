---
name: issues
description: "GitHub Issue / PR / 评论 / label 的 gh 命令编排单源（ship/fix/pr-review 共用）：建/改/关 issue、贴 PR 评论并回显 comment URL、PR 双轴状态 label 流转、PR 冲突预检 + CI watch。另含「非任务 issue 状态核查」：查代码判断 issue 是否仍成立，只判不修，建议 /ship 或 close。当用户要建/改 issue、给 PR 留评论、切 PR 状态 label、核一个 issue 是否还成立时使用。"
argument-hint: "<#issue（状态核查）| create-issue | edit-issue | comment | pr-status | pr-precheck> [...]"
allowed-tools: [Read, Grep, Bash, Agent, AskUserQuestion]
---

# issues — Issue / PR / 评论 gh 编排单源

> 本技能是 issue/PR/评论**固定 gh 命令形态的单源**——ship / fix / pr-review 引用本文，不重印命令。
> 仓库：`ghbvf/prmonitor`，base 分支 `develop`。
> 输入分派：**普通 issue 号** → 「非任务 issue 状态核查」；**动词**（create / edit / comment / pr-status / pr-precheck）→ 对应原子操作。
> create issue 前先 `gh issue list --search` 查重（幂等）。

---

## 非任务 issue 状态核查（查代码判状态，只判不修）

输入一个 issue 号、要核它是否仍成立时：

1. `gh issue view <N> --json title,body,labels` 读问题描述 + body 的 Files。
2. 按 Files / 关键字 Read/Grep 定位代码；跨 3+ 文件时并行派 `Agent(Explore)` 核查。
3. 判状态（**只判不修**）：**存在** / **已修复**（给证据：哪行 / 哪 PR）/ **已变更**（形态变化）/ **无法确认**。
4. 输出状态 + 证据 + 建议：需修 → 建议 `/ship #<N>`（或定位到 file:line 后 `/fix`）；已修复 / 过期 → 建议关闭（`gh issue close --reason ...`）。

---

## B1. 新建 issue

简单 issue（标题 + body，可选一个朴素 label，无四轴强制门）：

```bash
gh issue create --title "<简短标题>" --body-file <填好的 body.md>
# 可选：--label <name>（仓库已有的朴素 label，无则省略）
```

> 由 review/fix finding 派生成文时，body **无损**写入：现状（证据代码片段 + 三维根因 + 影响）/ 修复方向（最小/彻底/重构 三级方案种子）/ Files（file:line 全集）/ Source（`PR #<N> finding <Fk>`，派生注明 `Discovered via /ship|/fix #<N>`）。不得一句话带过——否则后续无法据此修复。

## B2. 编辑 / 关闭 issue

```bash
gh issue edit <N> --add-label <name> --remove-label <name>            # 改 label
gh issue close <N> --reason completed --comment "Fixed in PR #<NNN>"   # 修复闭合
gh issue close <N> --reason "not planned" --comment "<理由>"          # wontfix
```

## B3. PR 评论（编排）

> 评论格式分两类：**单 skill 专属模板**（`pm:ship` / `pm:fix` / `pm:pr-review`）内联在各自技能里；**跨 skill 共享模板**（`pm:oos` / `pm:ci`，ship+fix 共用）见本节末「共享评论模板」，引用方不重印。本节是**贴评论命令 + 回显 comment id** 的单源（ship/fix/pr-review 引用本节，不重印）：

```bash
URL=$(gh pr comment <N> --body-file <填好的评论 body>)   # stdout = https://github.com/ghbvf/prmonitor/pull/<N>#issuecomment-<id>
echo "✅ 已贴评论：$URL"                                   # 必须回显给用户；comment id = URL 尾段 #issuecomment-<id>
```

- stdout 即评论 URL（含 `#issuecomment-<id>`）——**贴完必须捕获并回显**，便于跳转 / 引用。
- 命令非 0 退出 → 报错退出，不静默跳过。
- footer 由贴评论方自填（PR#/工具/head 分支/worktree 路径/session id，拿不到填 `—`）。

### 共享评论模板（`pm:oos` / `pm:ci`，无机器块）

**`pm:oos`**（OUT_OF_SCOPE findings 从主评论分离的无损存档；每条已建 issue 或显式 deferred）：

```markdown
<!-- pm:oos -->
## 🚦 Out-of-Scope Findings

**OOS Findings** <k> 条（已从 pm:ship/pm:fix 主评论分离，本评论为无损存档；每条已建 issue 或显式 deferred）

**F3** [P2·small·可靠性] `path/to/z.rs:64`（🚦 OUT_OF_SCOPE）
- 证据：`<code 片段>`
- 三维根因：代码 <…> / 架构 <1 处局部｜Grep N 处系统性> / 历史 <git log 同类>
- 三级方案种子：最小 <…> / 彻底 <…> / 重构 <…>
- Files：`path/to/z.rs:64`
- 处置：✅ 已建 issue **#<N>** <url>（body 按 B1 无损填）｜🟡 deferred:<原因>

---
🤖 PR #<N> · Generated with Claude Code · branch <head 分支> · worktree <路径|—> · session <会话id|—>
```

**`pm:ci`**（CI 收敛结果，独立评论）：

```markdown
<!-- pm:ci -->
## CI 检查结果

**状态**：<通过 / 失败>（已通过 <n> / 共 <total> 个检查）

<若有失败>
**失败检查**：
- `<check-name>` — <url>

---
🤖 PR #<N> · Generated with Claude Code · branch <head 分支> · worktree <路径|—> · session <会话id|—>
```

> 两模板均**无机器块**（无尾部结构化块）。`pm:oos` 建单走 B1（朴素 title+body，无四轴 label）；incident/红线/归属不清 → 停下 AskUserQuestion，不静默自动建。

## B4. PR 冲突预检 + CI 跟进（ship/fix 共用）

push 后流程分**两阶段**：① 冲突预检（阻塞，贴评论前必过）→ 立即收尾（贴评论，不等 CI）→ ② CI 异步收敛（收尾后再跑）。

**① 冲突预检**（阻塞，必须先于收尾）：`gh pr view <N> --json mergeable,mergeStateStatus`。`mergeable` 由 GitHub **异步计算**，刚 push 常返回 `UNKNOWN`——**轮询几次（~5-10s 间隔）直到落定** `MERGEABLE` / `CONFLICTING`。`CONFLICTING`（或 `mergeStateStatus=DIRTY`）→ 先解冲突：`git -C <wt> fetch origin && git -C <wt> merge origin/develop --no-edit`（解冲突 → commit → push）→ 回本步重检。`MERGEABLE` → 进行收尾。

**② CI 异步收敛**（收尾之后再跑）：本仓 CI 两个 job（frontend：vue-tsc + vite build；rust：cargo fmt + clippy + build），typically 几分钟。

```bash
# 阻塞轮询直到所有 check 完成（exit 0=全绿 / 8=pending / 非0=有失败）
gh pr checks <N> --watch --interval 30 --fail-fast       # Bash timeout 设 ~600000ms（10min 工具上限）
# 失败 → 列失败 check + run 链接
gh pr checks <N> --json name,bucket,link --jq '.[] | select(.bucket=="fail") | [.name,.link] | @tsv'
gh run view <run-id> --job <job-id> --log-failed         # link 末段 job-id、中段 run-id
```

- 失败 → 回 `fix` 修复循环（定位 → 修 → commit → push → 重新预检），**最多 3 轮**；**3 轮仍红 → 停下交人工**（不无限等、不静默）。
- CI 收敛后在窗口报告结果（全绿 / 仍红 + 失败 check 摘要 + run 链接）。

## B5. PR 状态 label 流转（编排）

> 两正交轴的**切换命令单源**——ship/fix/pr-review 引用本节，不重印。**不变式**：PR 始终**恰好一个** `pr-status/*`；`pr-review` 轴 `approved` **XOR** `changes-requested`——**切一侧必 `--remove-label` 同轴对侧**。`/fix` 不能直接到 `ready`，必过 `/pr-review --check` 验证。`needs-review-again` 仅 ship 首审一次用；后续 review→changes-requested 始终切 `needs-fix`（5-state）。

| 轴 | label | 何时 |
|----|-------|------|
| pr-status（互斥） | `in-progress` | ship 创建 PR 时 |
| | `needs-review-again` | ship 收尾交接（首审唯一使用点） |
| | `needs-fix` | review 出 changes-requested |
| | `needs-check-fix` | /fix 修完，待 --check 验证 |
| | `ready` | --check 全修复（终态） |
| pr-review（XOR） | `approved` / `changes-requested` | review / --check 结论 |

```bash
# ship 创建 PR 后
gh pr edit <N> --add-label pr-status/in-progress
# ship 收尾交接（首审唯一使用点）
gh pr edit <N> --add-label pr-status/needs-review-again --remove-label pr-status/in-progress
# review 有 findings（5-state：始终切 needs-fix）
gh pr edit <N> --add-label pr-review/changes-requested --add-label pr-status/needs-fix \
  --remove-label pr-review/approved --remove-label pr-status/needs-review-again
# review 无 findings → 终态
gh pr edit <N> --add-label pr-review/approved --add-label pr-status/ready \
  --remove-label pr-review/changes-requested --remove-label pr-status/needs-review-again
# /fix 修完 → 待验证（fix 不能直接 ready）
gh pr edit <N> --add-label pr-status/needs-check-fix --remove-label pr-status/needs-fix
# --check 全修复 → 终态
gh pr edit <N> --add-label pr-status/ready --add-label pr-review/approved \
  --remove-label pr-status/needs-check-fix --remove-label pr-review/changes-requested
# --check 有未修/回归 → 回 fix
gh pr edit <N> --add-label pr-review/changes-requested --add-label pr-status/needs-fix \
  --remove-label pr-status/needs-check-fix --remove-label pr-review/approved
```

### round / 熔断（无机器块的确定性来源）

review↔fix 轮次 = 数 `pm:fix` 评论（每轮 fix 贴一条），机器可数，不靠人记：

```bash
gh pr view <N> --json comments --jq '[.comments[]|select(.body|contains("<!-- pm:fix -->"))]|length'
```

`round >= 3` → 窗口提示「review↔fix 已达 3 轮，建议转人工」。本仓**无自动 dispatch 循环**（人工驱动 review→fix→check），故熔断是**人读指引**而非机器门——无失控风险，可接受（不引入机器块即不实现机器级熔断）。

## B6. 沟通规则

- issue 编辑 / 评论 / label 流转 / 冲突预检 / CI watch：按流程自动执行，不逐条问。
- create issue 前查重；状态核查只判不修。
- 切 label 必同轴互斥（切一侧撤对侧）；round 由数 pm:fix 评论派生。
- CI 修复 3 轮仍红 → 停下交人工。
