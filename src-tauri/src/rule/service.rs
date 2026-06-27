use crate::config::service::{RuleActionKind, RuleConfig};
use crate::dispatch;
use crate::error::{AppError, AppResult};
use crate::model::{
    ActionKind, Candidate, Event, Notification, NotificationLevel, RedactedNotificationBody,
    ReviewActionPayload,
};

pub fn matches(rule: &RuleConfig, event: &Event) -> bool {
    if !rule.enabled {
        return false;
    }
    if rule.source.is_some_and(|source| source != event.source) {
        return false;
    }
    if rule
        .event_type
        .is_some_and(|event_type| event_type != event.event_type)
    {
        return false;
    }
    if !rule.project_id.is_empty() && rule.project_id != event.project_id {
        return false;
    }
    if !rule.repo.is_empty() && !rule.repo.eq_ignore_ascii_case(&event.repo) {
        return false;
    }
    if !rule.labels_any.is_empty()
        && !rule
            .labels_any
            .iter()
            .any(|wanted| event.labels.iter().any(|label| label == wanted))
    {
        return false;
    }
    if !rule
        .labels_all
        .iter()
        .all(|wanted| event.labels.iter().any(|label| label == wanted))
    {
        return false;
    }
    contains_ci(&event.title, &rule.title_contains) && contains_ci(&event.body, &rule.body_contains)
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
    pub payload: String,
    pub dedupe_key: String,
}

pub fn plan_event(
    rules: &[RuleConfig],
    event: &Event,
    candidate: Option<&Candidate>,
) -> Vec<RuleMatchPlan> {
    let mut plans = Vec::new();
    for rule in rules.iter().filter(|rule| matches(rule, event)) {
        let mut actions = Vec::new();
        let mut errors = Vec::new();
        for action in &rule.actions {
            match plan_action(rule, event, candidate, *action) {
                Ok(plan) => actions.push(plan),
                Err(e) => errors.push(e.message),
            }
        }
        plans.push(RuleMatchPlan {
            rule_id: rule.id.clone(),
            rule_name: rule.name.clone(),
            project_id: event.project_id.clone(),
            actions,
            errors,
        });
    }
    plans
}

fn plan_action(
    rule: &RuleConfig,
    event: &Event,
    candidate: Option<&Candidate>,
    action: RuleActionKind,
) -> AppResult<RuleActionPlan> {
    match action {
        RuleActionKind::Review => plan_review_like(rule, candidate, RuleActionKind::Review),
        RuleActionKind::Check => plan_review_like(rule, candidate, RuleActionKind::Check),
        RuleActionKind::Notify => plan_notification(rule, event),
    }
}

fn plan_review_like(
    rule: &RuleConfig,
    candidate: Option<&Candidate>,
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
    candidate.kind = candidate_kind.to_string();
    let payload = serde_json::to_string(&ReviewActionPayload {
        candidate: candidate.clone(),
    })
    .map_err(|e| AppError::new(format!("rule action 序列化失败：{e}")))?;
    let summary = format!(
        "Rule {} -> PR #{} {candidate_kind}",
        rule.name, candidate.number
    );
    let dedupe_key = dispatch::review_action_dedupe_key(&candidate);
    Ok(RuleActionPlan {
        kind: action_kind,
        summary,
        payload,
        dedupe_key,
    })
}

fn review_like_kinds(action: RuleActionKind) -> (&'static str, ActionKind) {
    match action {
        RuleActionKind::Review => ("review", ActionKind::Review),
        RuleActionKind::Check => ("check", ActionKind::Check),
        RuleActionKind::Notify => unreachable!("notify is not a review-like action"),
    }
}

fn plan_notification(rule: &RuleConfig, event: &Event) -> AppResult<RuleActionPlan> {
    let title = if let Some(number) = event.number {
        format!("Rule {} matched PR #{}", rule.name, number)
    } else {
        format!("Rule {} matched event", rule.name)
    };
    let note = Notification::new(
        NotificationLevel::Info,
        title.clone(),
        event.url.clone(),
        RedactedNotificationBody::fixed("Rule matched an inbound event"),
        event.project_id.clone(),
    );
    let payload = serde_json::to_string(&note)
        .map_err(|e| AppError::new(format!("rule notification 序列化失败：{e}")))?;
    let dedupe_key = format!("rule:{}:{}:notify", rule.id, event.dedupe_key);
    Ok(RuleActionPlan {
        kind: ActionKind::Notification,
        summary: title,
        payload,
        dedupe_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EventType, SourceKind};

    fn event() -> Event {
        Event {
            dedupe_key: "k".to_string(),
            source: SourceKind::Github,
            event_type: EventType::PullRequest,
            project_id: "p1".to_string(),
            repo: "Owner/Repo".to_string(),
            number: Some(7),
            title: "Fix Login".to_string(),
            body: "body text".to_string(),
            labels: vec!["ready".to_string(), "urgent".to_string()],
            url: "https://example.com".to_string(),
            received_at_epoch: 1,
        }
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
            actions: vec![RuleActionKind::Review],
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
            actions: vec![RuleActionKind::Notify],
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
            kind: "review".to_string(),
        }
    }

    #[test]
    fn plan_event_builds_review_check_and_notify_actions() {
        let mut r = rule();
        r.actions = vec![
            RuleActionKind::Review,
            RuleActionKind::Check,
            RuleActionKind::Notify,
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
        first.actions = vec![RuleActionKind::Review];
        let mut second = rule();
        second.id = "r2".to_string();
        second.actions = vec![RuleActionKind::Review];

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
}
