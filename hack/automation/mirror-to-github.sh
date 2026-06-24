#!/usr/bin/env bash
# mirror-to-github.sh — 把 Azure DevOps（权威源）定期镜像到 GitHub。
#
# 权威仓库在 Azure DevOps；GitHub 仓库是只读镜像（给 SonarCloud / 对外可见性用）。
# 本脚本维护一个独立的 bare 镜像目录（默认 ~/code/prmonitor-mirror），与你的开发 clone 分开：
#   fetch 走 Azure，push 走 GitHub，只同步分支 + tag（避开 refs/pull/*，不用 --mirror push）。
#
# 用法：
#   bash hack/automation/mirror-to-github.sh                # 全分支 + tag 同步
#   bash hack/automation/mirror-to-github.sh --branch develop   # 只同步 develop
#   bash hack/automation/mirror-to-github.sh --no-push      # 只创建/刷新本地镜像，不推 GitHub
#
# 前置（一次性）：GitHub 推送认证，二选一
#   gh auth login && gh auth setup-git            # 用 GitHub CLI（推荐）
#   或把 PRMONITOR_MIRROR_DST 设成 https://<PAT>@github.com/<slug>.git
#
# 可用环境变量覆盖默认：
#   PRMONITOR_MIRROR_DIR        镜像目录（默认 ~/code/prmonitor-mirror）
#   PRMONITOR_MIRROR_SRC        Azure 源 URL（默认从 forge.conf 拼）
#   PRMONITOR_MIRROR_DST        GitHub 目标 URL（默认 https://github.com/<GITHUB_REPO_SLUG>.git）
#   PRMONITOR_MIRROR_BRANCH     只同步某分支（等价 --branch）

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# 复用 forge.conf 的 forge 常量（ADO_ORG / ADO_PROJECT / ADO_REPO / GITHUB_REPO_SLUG），避免重复维护 URL。
if [[ -f "${SCRIPT_DIR}/forge.conf" ]]; then
  # shellcheck disable=SC1091
  source "${SCRIPT_DIR}/forge.conf"
fi

MIRROR_DIR="${PRMONITOR_MIRROR_DIR:-${HOME}/code/prmonitor-mirror}"
AZURE_URL="${PRMONITOR_MIRROR_SRC:-${ADO_ORG:-https://dev.azure.com/shengming0923}/${ADO_PROJECT:-prmonitor}/_git/${ADO_REPO:-prmonitor}}"
GITHUB_SLUG="${GITHUB_REPO_SLUG:-ghbvf/prmonitor}"
GITHUB_URL="${PRMONITOR_MIRROR_DST:-https://github.com/${GITHUB_SLUG}.git}"
BRANCH="${PRMONITOR_MIRROR_BRANCH:-}"
DO_PUSH=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    --branch) BRANCH="${2:?--branch 需要分支名}"; shift 2 ;;
    --no-push) DO_PUSH=0; shift ;;
    -h|--help) sed -n '2,33p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) printf '未知参数：%s（用 -h 看帮助）\n' "$1" >&2; exit 2 ;;
  esac
done

log()  { printf '\033[1;34m[mirror]\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m[mirror] %s\033[0m\n' "$*" >&2; exit 1; }

command -v git >/dev/null 2>&1 || die "git 未安装"

# --- 1. 确保 bare 镜像存在（首次 --mirror 克隆，含全部历史 + refs）---
if [[ ! -d "${MIRROR_DIR}" ]]; then
  log "首次创建镜像：${AZURE_URL} -> ${MIRROR_DIR}"
  mkdir -p "$(dirname "${MIRROR_DIR}")"
  git clone --mirror "${AZURE_URL}" "${MIRROR_DIR}"
fi

cd "${MIRROR_DIR}" || die "进不去镜像目录 ${MIRROR_DIR}"
[[ "$(git rev-parse --is-bare-repository 2>/dev/null)" == "true" ]] \
  || die "${MIRROR_DIR} 不是 bare 镜像仓库；删掉重建或改 PRMONITOR_MIRROR_DIR"

# origin(fetch)=Azure，由 clone --mirror 设好；关掉 origin 的 mirror-push 语义，推送只走 github 远端。
git config remote.origin.mirror false 2>/dev/null || true

# github 远端（只用于 push），幂等创建 / 纠正 URL。
if git remote get-url github >/dev/null 2>&1; then
  git remote set-url github "${GITHUB_URL}"
else
  git remote add github "${GITHUB_URL}"
fi

# --- 2. 从 Azure 拉最新（mirror clone 的 fetch refspec 是 +refs/*:refs/*）---
log "从 Azure 拉取：${AZURE_URL}"
git fetch --prune origin

if [[ "${DO_PUSH}" -eq 0 ]]; then
  log "已刷新本地镜像（--no-push）；GitHub 未推送。"
  exit 0
fi

# --- 3. 推到 GitHub：只推 heads + tags，--prune 同步删除，避开只读的 refs/pull/* ---
if [[ -n "${BRANCH}" ]]; then
  log "推送分支 ${BRANCH} -> github:${GITHUB_SLUG}"
  git push --force github "refs/heads/${BRANCH}:refs/heads/${BRANCH}" \
    || die "推送失败：先 gh auth login && gh auth setup-git（或用带 PAT 的 PRMONITOR_MIRROR_DST），并确认 GitHub 仓库 ${GITHUB_SLUG} 已存在且有写权限"
else
  log "推送全部分支 + tag -> github:${GITHUB_SLUG}"
  git push --prune github "+refs/heads/*:refs/heads/*" "+refs/tags/*:refs/tags/*" \
    || die "推送失败：先 gh auth login && gh auth setup-git（或用带 PAT 的 PRMONITOR_MIRROR_DST），并确认 GitHub 仓库 ${GITHUB_SLUG} 已存在且有写权限"
fi

log "完成。GitHub: https://github.com/${GITHUB_SLUG}"
