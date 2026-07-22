use crate::config::service::{
    Project, RuleActionConfig, RuleActionDedupePolicy, RuleActionTarget, RuleConfig,
};
use crate::error::{AppError, AppResult};
use crate::model::{
    ActionKind, Candidate, EventEnvelope, MessagingSendContent, NotificationLevel,
    ReviewActionPayload, SendMessagingRequest, SendNotificationRequest, SkillInvocation,
};

/// Resolve a skill invocation from project rules (manual UI / external ingress).
/// Prefers an enabled `runSkill` whose `extra_args` matches the request; otherwise the first
/// enabled `runSkill` for the project (skill template only); otherwise the built-in default skill.
/// Caller's validated `extra_args` always wins for the rendered command / skill_key.
pub fn resolve_skill_invocation(
    rules: &[RuleConfig],
    project: &Project,
    pr_number: u64,
    extra_args: &str,
) -> Result<SkillInvocation, String> {
    let extra_args = SkillInvocation::validate_extra_args(extra_args)?;
    let mut actions: Vec<&RuleActionConfig> = Vec::new();
    for rule in rules {
        if !rule.enabled {
            continue;
        }
        if !rule.project_id.is_empty() && rule.project_id != project.id {
            continue;
        }
        for action in &rule.actions {
            if let RuleActionConfig::RunSkill { enabled, .. } = action {
                if *enabled {
                    actions.push(action);
                }
            }
        }
    }
    let chosen = actions
        .iter()
        .find(|action| {
            matches!(
                action,
                RuleActionConfig::RunSkill {
                    extra_args: ea,
                    ..
                } if ea.trim() == extra_args
            )
        })
        .or_else(|| actions.first())
        .copied();
    match chosen {
        Some(RuleActionConfig::RunSkill {
            skill_name,
            skill_path,
            command_template,
            ..
        }) => SkillInvocation::from_config(
            &project.repo_root,
            skill_name,
            skill_path,
            command_template,
            pr_number,
            &project.repo,
            extra_args,
            project.engine_kind,
        ),
        Some(_) => unreachable!("only RunSkill actions are collected"),
        None => SkillInvocation::default_pr_review(
            &project.repo_root,
            pr_number,
            &project.repo,
            extra_args,
            project.engine_kind,
        ),
    }
}

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
    projects: &[Project],
    event: &EventEnvelope,
    candidate: Option<&Candidate>,
) -> Vec<RuleMatchPlan> {
    if let Some(request) = event.as_review_request() {
        let project = projects
            .iter()
            .find(|project| project.id == event.project_id());
        let invocation = match project {
            Some(project) => {
                resolve_skill_invocation(rules, project, request.pr_number, request.extra_args)
            }
            None => Err(format!(
                "external review request project not found: {}",
                event.project_id()
            )),
        };
        let invocation = match invocation {
            Ok(invocation) => invocation,
            Err(message) => {
                return vec![RuleMatchPlan {
                    rule_id: "system:external-review".to_string(),
                    rule_name: "External review request".to_string(),
                    project_id: event.project_id().to_string(),
                    actions: Vec::new(),
                    errors: vec![message],
                }];
            }
        };
        let payload = serde_json::to_string(&ReviewActionPayload::Explicit {
            pr_number: request.pr_number,
            request_id: request.request_id.clone(),
            origin: request.origin,
            invocation: invocation.clone(),
        })
        .expect("typed review request serializes");
        return vec![RuleMatchPlan {
            rule_id: "system:external-review".to_string(),
            rule_name: "External review request".to_string(),
            project_id: event.project_id().to_string(),
            actions: vec![RuleActionPlan {
                kind: ActionKind::RunSkill,
                summary: format!(
                    "External {} request for PR #{}",
                    invocation.skill_name, request.pr_number
                ),
                dispatch: RuleActionDispatch::Outbox { payload },
                dedupe_key: format!("external:{}", request.request_id),
                delay_secs: 0,
                action_id: invocation.skill_key.clone(),
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
            for action in rule.actions.iter().filter(|action| action.enabled()) {
                match plan_action(rule, projects, event, candidate, action) {
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
    projects: &[Project],
    event: &EventEnvelope,
    candidate: Option<&Candidate>,
    action: &RuleActionConfig,
) -> AppResult<RuleActionPlan> {
    let mut plan = match action {
        RuleActionConfig::RunSkill { .. } => {
            plan_run_skill(rule, projects, event, candidate, action)
        }
        RuleActionConfig::Notify { .. } => plan_notification(rule, event, action),
    }?;
    plan.delay_secs = action.delay_secs();
    plan.action_id = action.id().to_string();
    plan.level = action.level().to_string();
    Ok(plan)
}

fn plan_run_skill(
    rule: &RuleConfig,
    projects: &[Project],
    event: &EventEnvelope,
    candidate: Option<&Candidate>,
    action_config: &RuleActionConfig,
) -> AppResult<RuleActionPlan> {
    let RuleActionConfig::RunSkill {
        skill_name,
        skill_path,
        command_template,
        extra_args,
        ..
    } = action_config
    else {
        unreachable!("plan_run_skill only accepts RunSkill");
    };
    let Some(candidate) = candidate else {
        return Err(AppError::new(format!(
            "规则 {} 需要 PR candidate 才能触发 runSkill",
            rule.id
        )));
    };
    let project_id = if rule.project_id.is_empty() {
        event.project_id()
    } else {
        rule.project_id.as_str()
    };
    let project = projects
        .iter()
        .find(|project| project.id == project_id)
        .ok_or_else(|| AppError::new(format!("规则 {} 找不到项目 {project_id}", rule.id)))?;
    let invocation = SkillInvocation::from_config(
        &project.repo_root,
        skill_name,
        skill_path,
        command_template,
        candidate.number,
        event.repo(),
        extra_args,
        project.engine_kind,
    )
    .map_err(AppError::new)?;
    let mut candidate = candidate.clone();
    candidate.skill_key = invocation.skill_key.clone();
    let payload = serde_json::to_string(&ReviewActionPayload::Automatic {
        candidate: candidate.clone(),
        invocation: invocation.clone(),
    })
    .map_err(|e| AppError::new(format!("rule action 序列化失败：{e}")))?;
    let summary = format!(
        "Rule {} action {} -> PR #{} {}",
        rule.name,
        action_node_id(action_config),
        candidate.number,
        invocation.skill_name
    );
    let base_dedupe_key = crate::model::ReviewActionKey::for_candidate(&candidate)
        .map_err(AppError::new)?
        .into_inner();
    let dedupe_key = match action_config.dedupe_policy() {
        RuleActionDedupePolicy::Event => base_dedupe_key,
        RuleActionDedupePolicy::Action => {
            format!("rule:{}:{}:{base_dedupe_key}", rule.id, action_config.id())
        }
    };
    Ok(RuleActionPlan {
        kind: ActionKind::RunSkill,
        summary,
        dispatch: RuleActionDispatch::Outbox { payload },
        dedupe_key,
        delay_secs: action_config.delay_secs(),
        action_id: action_config.id().to_string(),
        level: action_config.level().to_string(),
    })
}

fn plan_notification(
    rule: &RuleConfig,
    event: &EventEnvelope,
    action: &RuleActionConfig,
) -> AppResult<RuleActionPlan> {
    let RuleActionConfig::Notify { target, .. } = action else {
        return Err(AppError::new("notify plan requires Notify action"));
    };
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
    let (kind, dispatch) = match target {
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
                        content: MessagingSendContent::Text { text },
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
        delay_secs: action.delay_secs(),
        action_id: action.id().to_string(),
        level: action.level().to_string(),
    })
}

fn rule_action_dedupe_key(
    rule: &RuleConfig,
    action: &RuleActionConfig,
    event: &EventEnvelope,
    suffix: &str,
) -> String {
    match action.dedupe_policy() {
        RuleActionDedupePolicy::Event => {
            format!("rule:{}:{}:{suffix}", rule.id, event.dedupe_key())
        }
        RuleActionDedupePolicy::Action => {
            format!(
                "rule:{}:{}:{}:{suffix}",
                rule.id,
                action.id(),
                event.dedupe_key()
            )
        }
    }
}

fn action_node_id(action: &RuleActionConfig) -> String {
    format!("{}:{}", action.level().trim(), action.id().trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        EventSubject, EventType, InboxDedupeKey, SourceKind, DEFAULT_COMMAND_TEMPLATE,
        DEFAULT_SKILL_NAME, DEFAULT_SKILL_PATH,
    };
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

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

    fn project_with_skill() -> Project {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("prmonitor-rule-skill-{n}"));
        let skill = dir.join(DEFAULT_SKILL_PATH);
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(&skill, "skill").unwrap();
        Project {
            id: "p1".to_string(),
            repo: "Owner/Repo".to_string(),
            repo_root: dir.to_string_lossy().into_owned(),
            ..Project::default()
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
            actions: vec![RuleActionConfig::run_skill("review")],
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
            actions: vec![RuleActionConfig::notify("notify")],
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
            skill_key: String::new(),
        }
    }

    #[test]
    fn plan_event_builds_run_skill_and_notify_actions() {
        let project = project_with_skill();
        let mut r = rule();
        r.actions = vec![
            RuleActionConfig::run_skill("review"),
            RuleActionConfig::run_skill_check("check"),
            RuleActionConfig::notify("notify"),
        ];
        let plans = plan_event(&[r], &[project], &event(), Some(&candidate()));

        assert_eq!(plans.len(), 1);
        assert!(plans[0].errors.is_empty(), "{:?}", plans[0].errors);
        assert_eq!(
            plans[0]
                .actions
                .iter()
                .map(|action| action.kind)
                .collect::<Vec<_>>(),
            vec![
                ActionKind::RunSkill,
                ActionKind::RunSkill,
                ActionKind::Notification
            ]
        );
        let review_key = SkillInvocation::skill_key(DEFAULT_SKILL_NAME, "");
        let check_key = SkillInvocation::skill_key(DEFAULT_SKILL_NAME, "--check");
        assert_eq!(
            plans[0].actions[0].dedupe_key,
            format!("7@abc123:{review_key}")
        );
        assert_eq!(
            plans[0].actions[1].dedupe_key,
            format!("7@abc123:{check_key}")
        );
        assert_eq!(plans[0].actions[2].dedupe_key, "rule:r1:k:notify");
    }

    #[test]
    fn review_like_actions_share_dispatch_dedupe_across_rules() {
        let project = project_with_skill();
        let mut first = rule();
        first.id = "r1".to_string();
        first.actions = vec![RuleActionConfig::run_skill("review")];
        let mut second = rule();
        second.id = "r2".to_string();
        second.actions = vec![RuleActionConfig::run_skill("review")];

        let plans = plan_event(&[first, second], &[project], &event(), Some(&candidate()));

        assert_eq!(plans.len(), 2);
        let review_key = SkillInvocation::skill_key(DEFAULT_SKILL_NAME, "");
        assert_eq!(
            plans[0].actions[0].dedupe_key,
            format!("7@abc123:{review_key}")
        );
        assert_eq!(
            plans[1].actions[0].dedupe_key,
            format!("7@abc123:{review_key}")
        );
    }

    #[test]
    fn plan_event_records_error_when_candidate_missing() {
        let project = project_with_skill();
        let plans = plan_event(&[rule()], &[project], &event(), None);

        assert_eq!(plans.len(), 1);
        assert!(plans[0].actions.is_empty());
        assert_eq!(plans[0].errors.len(), 1);
        assert!(plans[0].errors[0].contains("需要 PR candidate"));
    }

    #[test]
    fn plan_event_preserves_configured_action_order() {
        let project = project_with_skill();
        let mut r = rule();
        let notify = RuleActionConfig::notify("notify");
        r.actions = vec![notify, RuleActionConfig::run_skill("review")];

        let plans = plan_event(&[r], &[project], &event(), Some(&candidate()));

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
        let project = project_with_skill();
        let mut event_policy = rule();
        event_policy.id = "event-rule".to_string();
        event_policy.actions = vec![RuleActionConfig::run_skill("review")];
        let mut action_policy = rule();
        action_policy.id = "action-rule".to_string();
        let mut action = RuleActionConfig::run_skill("review");
        if let RuleActionConfig::RunSkill {
            ref mut dedupe_policy,
            ..
        } = action
        {
            *dedupe_policy = RuleActionDedupePolicy::Action;
        }
        action_policy.actions = vec![action];

        let plans = plan_event(
            &[event_policy, action_policy],
            &[project],
            &event(),
            Some(&candidate()),
        );

        let review_key = SkillInvocation::skill_key(DEFAULT_SKILL_NAME, "");
        assert_eq!(
            plans[0].actions[0].dedupe_key,
            format!("7@abc123:{review_key}")
        );
        assert_eq!(
            plans[1].actions[0].dedupe_key,
            format!("rule:action-rule:review:7@abc123:{review_key}")
        );
    }

    #[test]
    fn plan_notification_targets_channels_and_messaging_conversation() {
        let project = project_with_skill();
        let mut notify_rule = rule();
        let mut channel_action = RuleActionConfig::notify("notify");
        if let RuleActionConfig::Notify { ref mut target, .. } = channel_action {
            *target = RuleActionTarget::NotificationChannels {
                channel_ids: vec!["desktop".to_string()],
            };
        }
        let mut messaging_action = RuleActionConfig::notify("message");
        if let RuleActionConfig::Notify { ref mut target, .. } = messaging_action {
            *target = RuleActionTarget::MessagingConversation {
                integration_id: "feishu-main".to_string(),
                conversation_id: "oc_123".to_string(),
            };
        }
        notify_rule.actions = vec![channel_action, messaging_action];

        let plans = plan_event(&[notify_rule], &[project], &event(), Some(&candidate()));

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
                assert!(matches!(
                    &request.content,
                    MessagingSendContent::Text { text } if text.contains("action:message")
                ));
                assert_eq!(request.request_id, "rule:r1:k:notify");
            }
            RuleActionDispatch::Outbox { .. } | RuleActionDispatch::Notification { .. } => {
                panic!("expected messaging dispatch")
            }
        }
    }

    #[test]
    fn render_command_appends_extra_args_only_when_non_empty() {
        assert_eq!(
            SkillInvocation::render_command(
                DEFAULT_COMMAND_TEMPLATE,
                DEFAULT_SKILL_NAME,
                7,
                "owner/repo",
                ""
            ),
            "/pr-review 7"
        );
        assert_eq!(
            SkillInvocation::render_command(
                DEFAULT_COMMAND_TEMPLATE,
                DEFAULT_SKILL_NAME,
                7,
                "owner/repo",
                "--check"
            ),
            "/pr-review 7 --check"
        );
        assert_eq!(
            SkillInvocation::render_command(
                "/{skill} {pr} in {repo}",
                DEFAULT_SKILL_NAME,
                9,
                "a/b",
                "  "
            ),
            "/pr-review 9 in a/b"
        );
    }
}
