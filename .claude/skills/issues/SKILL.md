---
name: issues
description: "GitHub Issue / PR / 评论的 gh 命令编排单源（ship/fix/pr-review 共用）：建/改/关 issue、贴 PR 评论并回显 comment URL、PR 冲突预检 + CI watch。另含「非任务 issue 状态核查」：查代码判断 issue 是否仍成立，只判不修，建议 /ship 或 close。当用户要建/改 issue、给 PR 留评论、核一个 issue 是否还成立时使用。"
argument-hint: "<#issue（状态核查）| create-issue | edit-issue | comment | pr-precheck> [...]"
allowed-tools: [Read, Grep, Bash, Agent, AskUserQuestion]
---

# issues — Issue / PR / 评论 gh 编排单源

> 本技能是 issue/PR/评论**固定 gh 命令形态的单源**——ship / fix / pr-review 引用本文，不重印命令。
> 仓库：`ghbvf/prmonitor`，base 分支 `develop`。
> 输入分派：**普通 issue 号** → 「非任务 issue 状态核查」；**动词**（create / edit / comment / pr-precheck）→ 对应原子操作。
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

> 评论格式（`pm:ship` / `pm:fix` / `pm:pr-review` 模板）内联在各自技能里；本节是**贴评论命令 + 回显 comment id** 的单源（ship/fix/pr-review 引用本节，不重印）：

```bash
URL=$(gh pr comment <N> --body-file <填好的评论 body>)   # stdout = https://github.com/ghbvf/prmonitor/pull/<N>#issuecomment-<id>
echo "✅ 已贴评论：$URL"                                   # 必须回显给用户；comment id = URL 尾段 #issuecomment-<id>
```

- stdout 即评论 URL（含 `#issuecomment-<id>`）——**贴完必须捕获并回显**，便于跳转 / 引用。
- 命令非 0 退出 → 报错退出，不静默跳过。
- footer 由贴评论方自填（PR#/工具/head 分支/worktree 路径/session id，拿不到填 `—`）。

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

## B5. 沟通规则

- issue 编辑 / 评论 / 冲突预检 / CI watch：按流程自动执行，不逐条问。
- create issue 前查重；状态核查只判不修。
- CI 修复 3 轮仍红 → 停下交人工。
