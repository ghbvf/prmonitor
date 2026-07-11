import type { ReviewLifecycleNotificationConfig, ReviewLifecycleTarget, RuleActionConfig, RuleConfig } from "./types";

export function cloneRuleAction(action: RuleActionConfig): RuleActionConfig {
  return {
    ...action,
    target:
      action.target.kind === "notificationChannels"
        ? { ...action.target, channelIds: [...action.target.channelIds] }
        : { ...action.target },
  };
}

export function cloneRule(rule: RuleConfig): RuleConfig {
  return {
    ...rule,
    labelsAny: [...rule.labelsAny],
    labelsAll: [...rule.labelsAll],
    actions: rule.actions.map(cloneRuleAction),
    allowActionKinds: [...rule.allowActionKinds],
    denyActionKinds: [...rule.denyActionKinds],
  };
}

export function cloneReviewLifecycleTarget(target: ReviewLifecycleTarget): ReviewLifecycleTarget {
  switch (target.kind) {
    case "notificationChannels":
      return { ...target, channelIds: [...target.channelIds] };
    case "messagingConversation":
      return { ...target };
  }
}

export function cloneReviewLifecycleNotifications(
  config: ReviewLifecycleNotificationConfig,
): ReviewLifecycleNotificationConfig {
  return {
    ...config,
    events: [...config.events],
    targets: config.targets.map(cloneReviewLifecycleTarget),
  };
}
