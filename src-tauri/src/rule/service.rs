use crate::config::service::{
    RuleActionConfig, RuleActionDedupePolicy, RuleActionKind, RuleActionTarget, RuleConfig,
};
use crate::error::{AppError, AppResult};
use crate::model::{
    ActionKind, Candidate, EventEnvelope, NotificationLevel, ReviewActionPayload, ReviewKind,
    SendMessagingRequest, SendNotificationRequest,
};

pub fn matches(rule: &RuleConfig, event: &EventEnvelope) -> bool {
    let Some(observation) = event.as_observation() else {
        return false;
    };
    let subject = observation.subject;
    if !rule.enabled {
        return false;
    }
    if rule.source.is_some_and(|source| source != event.source()) {
        return false;
    }
    if rule
        .event_type
        .is_some_and(|event_type| event_type != observation.event_type)
    {
        return false;
    }
    if !rule.project_id.is_empty() && rule.project_id != event.project_id() {
        return false;
    }
    if !rule.repo.is_empty() && !rule.repo.eq_ignore_ascii_case(event.repo()) {
        return false;
    }
    if !rule.labels_any.is_empty()
        && !rule
            .labels_any
            .iter()
            .any(|wanted| subject.labels.iter().any(|label| label == wanted))
    {
        return false;
    }
    if !rule
        .labels_all
        .iter()
        .all(|wanted| subject.labels.iter().any(|label| label == wanted))
    {
        return false;
    }
    contains_ci(&subject.title, &rule.title_contains)
        && contains_ci(&subject.body, &rule.body_contains)
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    let needle = needle.trim();
    needle.is_empty() || haystack.to_lowercase().contains(&needle.to_lowercase())
}

#[derive(Debug, Clone)]
pub struct RuleMatchPlan {
    pub rule_id: String,
    pub rule_name: String,
    pub project_id: String,
    pub actions: Vec<RuleActionPlan>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RuleActionPlan {
    pub kind: ActionKind,
    pub summary: String,
    pub dispatch: RuleActionDispatch,
    pub dedupe_key: String,
    pub delay_secs: u64,
    pub action_id: String,
    pub level: String,
}

#[derive(Debug, Clone)]
pub enum RuleActionDispatch {
    Outbox { payload: String },
    Notification { request: SendNotificationRequest },
    Messaging { request: SendMessagingRequest },
}

pub fn plan_event(
    rules: &[RuleConfig],
    event: &EventEnvelope,
    candidate: Option<&Candidate>,
) -> Vec<RuleMatchPlan> {
    if let Some(request) = event.as_review_request() {
        let kind = match request.review_kind {
            ReviewKind::Review => ActionKind::Review,
            ReviewKind::Check => ActionKind::Check,
        };
        let payload = serde_json::to_string(&ReviewActionPayload::Explicit {
            pr_number: request.pr_number,
            request_id: request.request_id.clone(),
            origin: request.origin,
        })
        .expect("typed review request serializes");
        return vec![RuleMatchPlan {
            rule_id: "system:external-review".to_string(),
            rule_name: "External review request".to_string(),
            project_id: event.project_id().to_string(),
            actions: vec![RuleActionPlan {
                kind,
                summary: format!(
                    "External {} request for PR #{}",
                    request.review_kind, request.pr_number
                ),
                dispatch: RuleActionDispatch::Outbox { payload },
                dedupe_key: format!("external:{}", request.request_id),
                delay_secs: 0,
                action_id: request.review_kind.to_string(),
                level: "system".to_string(),
            }],
            errors: Vec::new(),
        }];
    }
    let mut plans = Vec::new();
    for rule in rules.iter().filter(|rule| matches(rule, event)) {
        let mut actions = Vec::new();
        let mut errors = Vec::new();
        {
            for action in rule.actions.iter().filter(|action| action.enabled) {
                match plan_action(rule, event, candidate, action) {
                    Ok(plan) => actions.push(plan),
                    Err(e) => errors.push(e.message),
                }
            }
        }
        plans.push(RuleMatchPlan {
            rule_id: rule.id.clone(),
            rule_name: rule.name.clone(),
            project_id: event.project_id().to_string(),
            actions,
            errors,
        });
    }
    plans
}

fn plan_action(
    rule: &RuleConfig,
    event: &EventEnvelope,
    candidate: Option<&Candidate>,
    action: &RuleActionConfig,
) -> AppResult<RuleActionPlan> {
    let mut plan = match action.kind {
        RuleActionKind::Review => plan_review_like(rule, candidate, action, RuleActionKind::Review),
        RuleActionKind::Check => plan_review_like(rule, candidate, action, RuleActionKind::Check),
        RuleActionKind::Notify => plan_notification(rule, event, action),
    }?;
    plan.delay_secs = action.delay_secs;
    plan.action_id = action.id.clone();
    plan.level = action.level.clone();
    Ok(plan)
}

fn plan_review_like(
    rule: &RuleConfig,
    candidate: Option<&Candidate>,
    action_config: &RuleActionConfig,
    action: RuleActionKind,
) -> AppResult<RuleActionPlan> {
    let (candidate_kind, action_kind) = review_like_kinds(action);
    let Some(candidate) = candidate else {
        return Err(AppError::new(format!(
            "规则 {} 需要 PR candidate 才能触发 {candidate_kind}",
            rule.id
        )));
    };
    let mut candidate = candidate.clone();
    candidate.kind = candidate_kind;
    let payload = serde_json::to_string(&ReviewActionPayload::Automatic {
        candidate: candidate.clone(),
    })
    .map_err(|e| AppError::new(format!("rule action 序列化失败：{e}")))?;
    let summary = format!(
        "Rule {} action {} -> PR #{} {candidate_kind}",
        rule.name,
        action_node_id(action_config),
        candidate.number
    );
    let base_dedupe_key = crate::model::ReviewActionKey::for_candidate(&candidate)
        .map_err(AppError::new)?
        .into_inner();
    let dedupe_key = match action_config.dedupe_policy {
        RuleActionDedupePolicy::Event => base_dedupe_key,
        RuleActionDedupePolicy::Action => {
            format!("rule:{}:{}:{base_dedupe_key}", rule.id, action_config.id)
        }
    };
    Ok(RuleActionPlan {
        kind: action_kind,
        summary,
        dispatch: RuleActionDispatch::Outbox { payload },
        dedupe_key,
        delay_secs: action_config.delay_secs,
        action_id: action_config.id.clone(),
        level: action_config.level.clone(),
    })
}

fn review_like_kinds(action: RuleActionKind) -> (ReviewKind, ActionKind) {
    match action {
        RuleActionKind::Review => (ReviewKind::Review, ActionKind::Review),
        RuleActionKind::Check => (ReviewKind::Check, ActionKind::Check),
        RuleActionKind::Notify => unreachable!("notify is not a review-like action"),
    }
}

fn plan_notification(
    rule: &RuleConfig,
    event: &EventEnvelope,
    action: &RuleActionConfig,
) -> AppResult<RuleActionPlan> {
    let node_id = action_node_id(action);
    let subject = event
        .as_observation()
        .expect("configuration rules only consume observations")
        .subject;
    let title = if let Some(number) = subject.number {
        format!("Rule {} action {node_id} matched PR #{}", rule.name, number)
    } else {
        format!("Rule {} action {node_id} matched event", rule.name)
    };
    let body = format!("Rule matched an inbound event\nnodeId: {node_id}");
    let dedupe_key = rule_action_dedupe_key(rule, action, event, "notify");
    let (kind, dispatch) = match &action.target {
        RuleActionTarget::None => (
            ActionKind::Notification,
            RuleActionDispatch::Notification {
                request: SendNotificationRequest {
                    level: Some(NotificationLevel::Info),
                    title: title.clone(),
                    body: Some(body),
                    url: Some(subject.url.clone()).filter(|url| !url.is_empty()),
                    project_id: Some(event.project_id().to_string()),
                    channel_ids: Vec::new(),
                },
            },
        ),
        RuleActionTarget::NotificationChannels { channel_ids } => (
            ActionKind::Notification,
            RuleActionDispatch::Notification {
                request: SendNotificationRequest {
                    level: Some(NotificationLevel::Info),
                    title: title.clone(),
                    body: Some(body),
                    url: Some(subject.url.clone()).filter(|url| !url.is_empty()),
                    project_id: Some(event.project_id().to_string()),
                    channel_ids: channel_ids.clone(),
                },
            },
        ),
        RuleActionTarget::MessagingConversation {
            integration_id,
            conversation_id,
        } => {
            let text = if subject.url.is_empty() {
                format!("{title}\n{body}")
            } else {
                format!("{title}\n{body}\n{}", subject.url)
            };
            (
                ActionKind::MessagingSend,
                RuleActionDispatch::Messaging {
                    request: SendMessagingRequest {
                        integration_id: integration_id.clone(),
                        conversation_id: conversation_id.clone(),
                        text,
                        request_id: dedupe_key.clone(),
                    },
                },
            )
        }
    };
    Ok(RuleActionPlan {
        kind,
        summary: title,
        dispatch,
        dedupe_key,
        delay_secs: action.delay_secs,
        action_id: action.id.clone(),
        level: action.level.clone(),
    })
}

fn rule_action_dedupe_key(
    rule: &RuleConfig,
    action: &RuleActionConfig,
    event: &EventEnvelope,
    suffix: &str,
) -> String {
    match action.dedupe_policy {
        RuleActionDedupePolicy::Event => {
            format!("rule:{}:{}:{suffix}", rule.id, event.dedupe_key())
        }
        RuleActionDedupePolicy::Action => {
            format!(
                "rule:{}:{}:{}:{suffix}",
                rule.id,
                action.id,
                event.dedupe_key()
            )
        }
    }
}

fn action_node_id(action: &RuleActionConfig) -> String {
    format!("{}:{}", action.level.trim(), action.id.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EventSubject, EventType, InboxDedupeKey, SourceKind};

    fn event() -> EventEnvelope {
        EventEnvelope::observation(
            InboxDedupeKey::new("k").unwrap(),
            SourceKind::Github,
            "p1",
            "Owner/Repo",
            EventType::PullRequest,
            EventSubject {
                number: Some(7),
                title: "Fix Login".to_string(),
                body: "body text".to_string(),
                labels: vec!["ready".to_string(), "urgent".to_string()],
                url: "https://example.com".to_string(),
            },
            1,
        )
        .unwrap()
    }

    fn rule() -> RuleConfig {
        RuleConfig {
            id: "r1".to_string(),
            name: "Ready".to_string(),
            enabled: true,
            source: Some(SourceKind::Github),
            event_type: Some(EventType::PullRequest),
            project_id: "p1".to_string(),
            repo: "owner/repo".to_string(),
            labels_any: vec!["ready".to_string()],
            labels_all: vec!["urgent".to_string()],
            title_contains: "login".to_string(),
            body_contains: "BODY".to_string(),
            actions: vec![RuleActionConfig::new("review", RuleActionKind::Review)],
            allow_action_kinds: Vec::new(),
            deny_action_kinds: Vec::new(),
        }
    }

    #[test]
    fn matcher_covers_fields_and_case_insensitive_text() {
        assert!(matches(&rule(), &event()));
    }

    #[test]
    fn disabled_rule_does_not_match() {
        let mut r = rule();
        r.enabled = false;
        assert!(!matches(&r, &event()));
    }

    #[test]
    fn empty_matchers_are_wildcards() {
        let r = RuleConfig {
            id: "r".to_string(),
            name: "all".to_string(),
            enabled: true,
            actions: vec![RuleActionConfig::new("notify", RuleActionKind::Notify)],
            ..RuleConfig::default()
        };
        assert!(matches(&r, &event()));
    }

    fn candidate() -> Candidate {
        Candidate {
            number: 7,
            head_sha: "abc123".to_string(),
            head_ref: "feature/login".to_string(),
            author: "dev".to_string(),
            is_cross_repository: false,
            is_draft: false,
            kind: ReviewKind::Review,
        }
    }

    #[test]
    fn plan_event_builds_review_check_and_notify_actions() {
        let mut r = rule();
        r.actions = vec![
            RuleActionConfig::new("review", RuleActionKind::Review),
            RuleActionConfig::new("check", RuleActionKind::Check),
            RuleActionConfig::new("notify", RuleActionKind::Notify),
        ];
        let plans = plan_event(&[r], &event(), Some(&candidate()));

        assert_eq!(plans.len(), 1);
        assert!(plans[0].errors.is_empty());
        assert_eq!(
            plans[0]
                .actions
                .iter()
                .map(|action| action.kind)
                .collect::<Vec<_>>(),
            vec![
                ActionKind::Review,
                ActionKind::Check,
                ActionKind::Notification
            ]
        );
        assert_eq!(plans[0].actions[0].dedupe_key, "7@abc123:review");
        assert_eq!(plans[0].actions[1].dedupe_key, "7@abc123:check");
        assert_eq!(plans[0].actions[2].dedupe_key, "rule:r1:k:notify");
    }

    #[test]
    fn review_like_actions_share_dispatch_dedupe_across_rules() {
        let mut first = rule();
        first.id = "r1".to_string();
        first.actions = vec![RuleActionConfig::new("review", RuleActionKind::Review)];
        let mut second = rule();
        second.id = "r2".to_string();
        second.actions = vec![RuleActionConfig::new("review", RuleActionKind::Review)];

        let plans = plan_event(&[first, second], &event(), Some(&candidate()));

        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].actions[0].dedupe_key, "7@abc123:review");
        assert_eq!(plans[1].actions[0].dedupe_key, "7@abc123:review");
    }

    #[test]
    fn plan_event_records_error_when_candidate_missing() {
        let plans = plan_event(&[rule()], &event(), None);

        assert_eq!(plans.len(), 1);
        assert!(plans[0].actions.is_empty());
        assert_eq!(plans[0].errors.len(), 1);
        assert!(plans[0].errors[0].contains("需要 PR candidate"));
    }

    #[test]
    fn plan_event_preserves_configured_action_order() {
        let mut r = rule();
        let notify = RuleActionConfig::new("notify", RuleActionKind::Notify);
        r.actions = vec![
            notify,
            RuleActionConfig::new("review", RuleActionKind::Review),
        ];

        let plans = plan_event(&[r], &event(), Some(&candidate()));

        assert_eq!(
            plans[0]
                .actions
                .iter()
                .map(|action| action.action_id.as_str())
                .collect::<Vec<_>>(),
            vec!["notify", "review"]
        );
    }

    #[test]
    fn plan_event_consumes_action_dedupe_policy() {
        let mut event_policy = rule();
        event_policy.id = "event-rule".to_string();
        event_policy.actions = vec![RuleActionConfig::new("review", RuleActionKind::Review)];
        let mut action_policy = rule();
        action_policy.id = "action-rule".to_string();
        let mut action = RuleActionConfig::new("review", RuleActionKind::Review);
        action.dedupe_policy = RuleActionDedupePolicy::Action;
        action_policy.actions = vec![action];

        let plans = plan_event(&[event_policy, action_policy], &event(), Some(&candidate()));

        assert_eq!(plans[0].actions[0].dedupe_key, "7@abc123:review");
        assert_eq!(
            plans[1].actions[0].dedupe_key,
            "rule:action-rule:review:7@abc123:review"
        );
    }

    #[test]
    fn plan_notification_targets_channels_and_messaging_conversation() {
        let mut notify_rule = rule();
        let mut channel_action = RuleActionConfig::new("notify", RuleActionKind::Notify);
        channel_action.target = RuleActionTarget::NotificationChannels {
            channel_ids: vec!["desktop".to_string()],
        };
        let mut messaging_action = RuleActionConfig::new("message", RuleActionKind::Notify);
        messaging_action.target = RuleActionTarget::MessagingConversation {
            integration_id: "feishu-main".to_string(),
            conversation_id: "oc_123".to_string(),
        };
        notify_rule.actions = vec![channel_action, messaging_action];

        let plans = plan_event(&[notify_rule], &event(), Some(&candidate()));

        assert_eq!(plans[0].actions.len(), 2);
        match &plans[0].actions[0].dispatch {
            RuleActionDispatch::Notification { request } => {
                assert_eq!(request.channel_ids, vec!["desktop"]);
                assert!(request.title.contains("action:notify"));
                assert!(request
                    .body
                    .as_deref()
                    .is_some_and(|body| body.contains("nodeId: action:notify")));
            }
            RuleActionDispatch::Outbox { .. } | RuleActionDispatch::Messaging { .. } => {
                panic!("expected notification dispatch")
            }
        }
        match &plans[0].actions[1].dispatch {
            RuleActionDispatch::Messaging { request } => {
                assert_eq!(request.integration_id, "feishu-main");
                assert_eq!(request.conversation_id, "oc_123");
                assert!(request.text.contains("action:message"));
                assert_eq!(request.request_id, "rule:r1:k:notify");
            }
            RuleActionDispatch::Outbox { .. } | RuleActionDispatch::Notification { .. } => {
                panic!("expected messaging dispatch")
            }
        }
    }
}
