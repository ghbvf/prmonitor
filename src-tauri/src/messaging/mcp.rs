//! Loopback Streamable HTTP MCP server for first-wins human input (#1810).

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::Router;
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{
    ClientResult, ElicitRequest, ElicitRequestParams, ElicitationAction, ElicitationSchema,
    ServerCapabilities, ServerInfo, ServerRequest,
};
use rmcp::service::{PeerRequestOptions, RequestContext, RequestHandle};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{tool, tool_handler, tool_router, Json, RoleServer, ServerHandler};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use subtle::ConstantTimeEq;
use tauri::Manager;
use tower_http::limit::RequestBodyLimitLayer;

use crate::config::service as config_service;
use crate::config::service::MessagingIntegration;
use crate::db::Database;
use crate::messaging::dingtalk::{
    send_human_input_card as send_dingtalk_human_input_card,
    update_human_input_card as update_dingtalk_human_input_card,
};
use crate::messaging::feishu::{
    send_human_input_card as send_feishu_human_input_card,
    update_human_input_card as update_feishu_human_input_card,
};
use crate::messaging::human_input::{
    self, HumanAnswer, HumanAnswerSource, HumanInputBroker, HumanInputRequest, HumanInputStatus,
    HumanQuestion,
};
use crate::messaging::store;
use crate::model::MessagingProviderKind;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AskViaMessagingInput {
    pub purpose: String,
    pub title: String,
    pub message: String,
    pub questions: Vec<HumanQuestion>,
    #[serde(default)]
    pub context: Value,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Optional explicit selector; omitted uses the first enabled matching integration.
    pub integration_id: Option<String>,
    pub conversation_id: Option<String>,
}

pub type AskViaFeishuInput = AskViaMessagingInput;
pub type AskViaDingTalkInput = AskViaMessagingInput;

fn default_timeout() -> u64 {
    human_input::MAX_WAIT_SECONDS
}

const MAX_MCP_BODY_BYTES: usize = 64 * 1024;
const SESSION_CLOSE_GRACE_SECONDS: u64 = 300;

#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AskViaMessagingOutput {
    pub request_id: String,
    pub status: HumanInputStatus,
    pub source: Option<HumanAnswerSource>,
    pub answers: Vec<HumanAnswer>,
}

pub type AskViaFeishuOutput = AskViaMessagingOutput;
pub type AskViaDingTalkOutput = AskViaMessagingOutput;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CodexAnswer {
    answer: String,
    #[serde(default)]
    answer2: Option<String>,
    #[serde(default)]
    answer3: Option<String>,
}
rmcp::elicit_safe!(CodexAnswer);

#[derive(Clone)]
struct PrmonitorMcp<R: tauri::Runtime> {
    app: tauri::AppHandle<R>,
    #[expect(
        dead_code,
        reason = "tool_handler macro dispatches through this generated router"
    )]
    tool_router: ToolRouter<Self>,
}

impl<R: tauri::Runtime> PrmonitorMcp<R> {
    fn new(app: tauri::AppHandle<R>) -> Self {
        Self {
            app,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl<R: tauri::Runtime> PrmonitorMcp<R> {
    #[tool(
        name = "ask_via_feishu",
        description = "Ask 1-3 human questions through both Feishu and a Codex elicitation popup; the first answer wins atomically."
    )]
    async fn ask_via_feishu(
        &self,
        Parameters(input): Parameters<AskViaFeishuInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<Json<AskViaFeishuOutput>, String> {
        self.ask_via_messaging(MessagingProviderKind::Feishu, input, context)
            .await
    }

    #[tool(
        name = "ask_via_dingtalk",
        description = "Ask 1-3 human questions through both DingTalk interactive cards and a Codex elicitation popup; the first answer wins atomically."
    )]
    async fn ask_via_dingtalk(
        &self,
        Parameters(input): Parameters<AskViaDingTalkInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<Json<AskViaDingTalkOutput>, String> {
        self.ask_via_messaging(MessagingProviderKind::DingTalk, input, context)
            .await
    }

    async fn ask_via_messaging(
        &self,
        kind: MessagingProviderKind,
        input: AskViaMessagingInput,
        context: RequestContext<RoleServer>,
    ) -> Result<Json<AskViaMessagingOutput>, String> {
        let label = provider_label(kind);
        if input.questions.is_empty() || input.questions.len() > 3 {
            return Err("questions 必须包含 1-3 个问题".into());
        }
        let cfg = config_service::load(&self.app).map_err(|e| e.message)?;
        let integration = cfg
            .messaging
            .integrations
            .into_iter()
            .find(|item| {
                item.enabled
                    && item.kind == kind
                    && input
                        .integration_id
                        .as_deref()
                        .is_none_or(|id| id == item.id)
            })
            .ok_or_else(|| format!("没有已启用且匹配的 {label} 消息集成"))?;
        let conversation_id = input
            .conversation_id
            .as_deref()
            .or_else(|| {
                integration
                    .allowed_conversation_ids
                    .first()
                    .map(String::as_str)
            })
            .ok_or_else(|| format!("{label} 消息集成没有允许的会话"))?
            .to_string();
        if !integration
            .allowed_conversation_ids
            .iter()
            .any(|id| id == &conversation_id)
        {
            return Err("conversationId 未授权".into());
        }
        let now = store::now_epoch();
        let timeout_secs = input.timeout_secs;
        if !(1..=human_input::MAX_WAIT_SECONDS).contains(&timeout_secs) {
            return Err(format!(
                "timeoutSecs 必须在 1-{} 秒之间",
                human_input::MAX_WAIT_SECONDS
            ));
        }
        let request = HumanInputRequest {
            id: format!("Q-{}", uuid::Uuid::new_v4().simple()),
            integration_id: integration.id.clone(),
            conversation_id: conversation_id.clone(),
            purpose: input.purpose,
            title: input.title,
            message: input.message,
            questions: input.questions,
            context: input.context,
            status: HumanInputStatus::Pending,
            answer: None,
            answer_source: None,
            card_message_id: None,
            created_at_epoch: now,
            expires_at_epoch: now.saturating_add(timeout_secs),
            answered_at_epoch: None,
        };
        let db = self.app.state::<Database>();
        let broker = self.app.state::<HumanInputBroker>();
        human_input::create(db.inner(), &request).map_err(|e| e.message)?;
        let mut receiver = broker.subscribe(&request.id);
        let message_id =
            match send_human_input_card(kind, &integration, &conversation_id, &request).await {
                Ok(message_id) => message_id,
                Err(error) => {
                    let _ = human_input::cancel(
                        db.inner(),
                        broker.inner(),
                        &request.id,
                        store::now_epoch(),
                    );
                    return Err(error.message);
                }
            };
        human_input::set_card_message_id(db.inner(), &request.id, &message_id)
            .map_err(|e| e.message)?;

        let question_text = request
            .questions
            .iter()
            .enumerate()
            .map(|(index, question)| {
                let options = if question.options.is_empty() {
                    String::new()
                } else {
                    format!("（选项：{}）", question.options.join(" / "))
                };
                format!("{}. {}{}", index + 1, question.question, options)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "{}\n\n{}\n\n{}\n\nRequest: {}\n依次填写 answer、answer2、answer3（未使用的字段可留空）。",
            request.title, request.message, question_text, request.id
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        let result = if !context.peer.supported_elicitation_modes().is_empty() {
            let handle = match ElicitationSchema::from_type::<CodexAnswer>() {
                Ok(schema) => {
                    let request_message = ServerRequest::ElicitRequest(ElicitRequest::new(
                        ElicitRequestParams::FormElicitationParams {
                            meta: None,
                            message: prompt,
                            requested_schema: schema,
                        },
                    ));
                    context
                        .peer
                        .send_cancellable_request(
                            request_message,
                            PeerRequestOptions::with_timeout(Duration::from_secs(timeout_secs)),
                        )
                        .await
                        .map_err(|error| format!("Codex elicitation 启动失败: {error}"))
                }
                Err(error) => Err(format!("Codex elicitation schema 非法: {error}")),
            };
            match handle {
                Ok(handle) => {
                    wait_with_elicitation(
                        db.inner(),
                        broker.inner(),
                        &request,
                        &mut receiver,
                        handle,
                        deadline,
                    )
                    .await?
                }
                Err(error) => {
                    eprintln!("{error}；继续等待消息通道回答（{}）", request.id);
                    wait_channel_only(
                        db.inner(),
                        broker.inner(),
                        &request.id,
                        &mut receiver,
                        deadline,
                    )
                    .await?
                }
            }
        } else {
            wait_channel_only(
                db.inner(),
                broker.inner(),
                &request.id,
                &mut receiver,
                deadline,
            )
            .await?
        };
        if let Some(card_message_id) = result
            .card_message_id
            .as_deref()
            .or(Some(message_id.as_str()))
        {
            if let Err(error) =
                update_human_input_card(kind, &integration, card_message_id, &result).await
            {
                eprintln!(
                    "更新{label}人工输入卡片失败（{}）：{}",
                    result.id, error.message
                );
            }
        }
        Ok(Json(AskViaMessagingOutput {
            request_id: result.id,
            status: result.status,
            source: result.answer_source,
            answers: result.answer.unwrap_or_default(),
        }))
    }
}

fn provider_label(kind: MessagingProviderKind) -> &'static str {
    match kind {
        MessagingProviderKind::Feishu => "Feishu",
        MessagingProviderKind::DingTalk => "钉钉",
        MessagingProviderKind::WeChatWork => "企业微信",
    }
}

async fn send_human_input_card(
    kind: MessagingProviderKind,
    integration: &MessagingIntegration,
    conversation_id: &str,
    request: &HumanInputRequest,
) -> crate::error::AppResult<String> {
    match kind {
        MessagingProviderKind::Feishu => {
            send_feishu_human_input_card(integration, conversation_id, request).await
        }
        MessagingProviderKind::DingTalk => {
            send_dingtalk_human_input_card(integration, conversation_id, request).await
        }
        MessagingProviderKind::WeChatWork => {
            Err(crate::error::AppError::new("企业微信不支持人工输入卡片"))
        }
    }
}

async fn update_human_input_card(
    kind: MessagingProviderKind,
    integration: &MessagingIntegration,
    card_message_id: &str,
    request: &HumanInputRequest,
) -> crate::error::AppResult<()> {
    match kind {
        MessagingProviderKind::Feishu => {
            update_feishu_human_input_card(integration, card_message_id, request).await
        }
        MessagingProviderKind::DingTalk => {
            update_dingtalk_human_input_card(integration, card_message_id, request).await
        }
        MessagingProviderKind::WeChatWork => {
            Err(crate::error::AppError::new("企业微信不支持人工输入卡片"))
        }
    }
}

async fn wait_with_elicitation(
    db: &Database,
    broker: &HumanInputBroker,
    request: &HumanInputRequest,
    receiver: &mut tokio::sync::watch::Receiver<Option<HumanInputRequest>>,
    mut handle: RequestHandle<RoleServer>,
    deadline: tokio::time::Instant,
) -> Result<HumanInputRequest, String> {
    tokio::select! {
        changed = receiver.changed() => {
            changed.map_err(|_| "Messaging answer channel closed".to_string())?;
            let winner = receiver.borrow().clone().ok_or_else(|| "Messaging answer missing".to_string())?;
            if let Err(error) = handle.cancel(Some("Messaging channel answered first".into())).await {
                eprintln!("取消 Codex elicitation 失败（{}）：{error}", request.id);
            }
            Ok(winner)
        }
        response = &mut handle.rx => {
            match response {
                Ok(Ok(ClientResult::ElicitResult(result))) => {
                    match result.action {
                        ElicitationAction::Accept => {
                            accept_codex_or_wait_channel(
                                db,
                                broker,
                                request,
                                receiver,
                                result.content,
                                deadline,
                            )
                            .await
                        }
                        ElicitationAction::Decline => {
                            cancel_after_codex_decline(db, broker, &request.id)
                        }
                        ElicitationAction::Cancel => {
                            continue_after_codex_cancel(
                                db,
                                broker,
                                request,
                                receiver,
                                deadline,
                            )
                            .await
                        }
                        _ => {
                            eprintln!("Codex elicitation 返回了未知 action（{}）；继续等待消息通道回答", request.id);
                            wait_channel_only(db, broker, &request.id, receiver, deadline).await
                        }
                    }
                }
                Ok(Ok(_)) => {
                    eprintln!("Codex elicitation 返回了意外响应（{}）；继续等待消息通道回答", request.id);
                    wait_channel_only(db, broker, &request.id, receiver, deadline).await
                }
                Ok(Err(error)) => {
                    eprintln!("Codex elicitation 失败（{}）：{error}", request.id);
                    wait_channel_only(db, broker, &request.id, receiver, deadline).await
                }
                Err(_) => wait_channel_only(db, broker, &request.id, receiver, deadline).await,
            }
        }
        _ = tokio::time::sleep_until(deadline) => {
            if let Err(error) = handle.cancel(Some("human input timeout".into())).await {
                eprintln!("超时取消 Codex elicitation 失败（{}）：{error}", request.id);
            }
            expire_request(db, broker, &request.id)
        }
    }
}

async fn continue_after_codex_cancel(
    db: &Database,
    broker: &HumanInputBroker,
    request: &HumanInputRequest,
    receiver: &mut tokio::sync::watch::Receiver<Option<HumanInputRequest>>,
    deadline: tokio::time::Instant,
) -> Result<HumanInputRequest, String> {
    eprintln!(
        "Codex elicitation Cancel（{}）；继续等待消息通道回答",
        request.id
    );
    wait_channel_only(db, broker, &request.id, receiver, deadline).await
}

fn cancel_after_codex_decline(
    db: &Database,
    broker: &HumanInputBroker,
    request_id: &str,
) -> Result<HumanInputRequest, String> {
    eprintln!("Codex elicitation Decline（{request_id}）；取消人工输入请求");
    human_input::cancel(db, broker, request_id, store::now_epoch())
        .map_err(|error| error.message)?;
    current_request(db, request_id)
}

async fn accept_codex_or_wait_channel(
    db: &Database,
    broker: &HumanInputBroker,
    request: &HumanInputRequest,
    receiver: &mut tokio::sync::watch::Receiver<Option<HumanInputRequest>>,
    content: Option<Value>,
    deadline: tokio::time::Instant,
) -> Result<HumanInputRequest, String> {
    let answer_result = parse_codex_accept(request, content).and_then(|answers| {
        human_input::answer(
            db,
            broker,
            &request.id,
            &answers,
            HumanAnswerSource::Codex,
            store::now_epoch(),
        )
        .map_err(|error| error.message)
    });
    match answer_result {
        Ok(_) => current_request(db, &request.id),
        Err(error) => {
            eprintln!(
                "Codex elicitation 答案无效（{}）：{error}；继续等待消息通道回答",
                request.id
            );
            wait_channel_only(db, broker, &request.id, receiver, deadline).await
        }
    }
}

fn parse_codex_accept(
    request: &HumanInputRequest,
    content: Option<Value>,
) -> Result<Vec<HumanAnswer>, String> {
    let value = content.ok_or_else(|| "Codex elicitation 缺少 content".to_string())?;
    let value = serde_json::from_value::<CodexAnswer>(value)
        .map_err(|error| format!("Codex elicitation 答案损坏: {error}"))?;
    let values = [Some(value.answer), value.answer2, value.answer3];
    let answers = request
        .questions
        .iter()
        .zip(values)
        .filter_map(|(question, answer)| {
            answer
                .filter(|value| !value.trim().is_empty())
                .map(|answer| HumanAnswer {
                    question_id: question.id.clone(),
                    answer,
                })
        })
        .collect::<Vec<_>>();
    if answers.len() != request.questions.len() {
        return Err("Codex elicitation 必须完整回答每一个问题".into());
    }
    Ok(answers)
}

async fn wait_channel_only(
    db: &Database,
    broker: &HumanInputBroker,
    request_id: &str,
    receiver: &mut tokio::sync::watch::Receiver<Option<HumanInputRequest>>,
    deadline: tokio::time::Instant,
) -> Result<HumanInputRequest, String> {
    tokio::select! {
        changed = receiver.changed() => {
            changed.map_err(|_| "messaging answer channel closed".to_string())?;
            receiver.borrow().clone().ok_or_else(|| "messaging answer missing".to_string())
        }
        _ = tokio::time::sleep_until(deadline) => expire_request(db, broker, request_id),
    }
}

fn expire_request(
    db: &Database,
    broker: &HumanInputBroker,
    request_id: &str,
) -> Result<HumanInputRequest, String> {
    human_input::expire(db, broker, request_id, store::now_epoch())
        .map_err(|error| error.message)?;
    current_request(db, request_id)
}

fn current_request(db: &Database, request_id: &str) -> Result<HumanInputRequest, String> {
    human_input::get(db, request_id)
        .map_err(|error| error.message)?
        .ok_or_else(|| "human input request disappeared".to_string())
}

pub(crate) async fn update_terminal_card<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    request: &HumanInputRequest,
) {
    let Some(message_id) = request.card_message_id.as_deref() else {
        return;
    };
    let integration = match config_service::messaging_integration(app, &request.integration_id) {
        Ok(integration) => integration,
        Err(error) => {
            eprintln!("恢复人工输入卡片失败（{}）：{}", request.id, error.message);
            return;
        }
    };
    let result = match integration.kind {
        MessagingProviderKind::Feishu => {
            update_feishu_human_input_card(&integration, message_id, request).await
        }
        MessagingProviderKind::DingTalk => {
            update_dingtalk_human_input_card(&integration, message_id, request).await
        }
        MessagingProviderKind::WeChatWork => return,
    };
    if let Err(error) = result {
        eprintln!("更新人工输入卡片失败（{}）：{}", request.id, error.message);
    }
}

#[tool_handler]
impl<R: tauri::Runtime> ServerHandler for PrmonitorMcp<R> {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}

#[derive(Clone)]
struct AuthState<R: tauri::Runtime> {
    app: tauri::AppHandle<R>,
}

pub(crate) fn build_router<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> Router {
    build_router_with_session_manager(app, Arc::new(human_input_session_manager()))
}

fn human_input_session_manager() -> LocalSessionManager {
    let mut manager = LocalSessionManager::default();
    manager.session_config.keep_alive = Some(Duration::from_secs(
        human_input::MAX_WAIT_SECONDS + SESSION_CLOSE_GRACE_SECONDS,
    ));
    manager
}

/// Session management is injected at the router boundary so lifecycle behavior can be exercised
/// independently of Tauri composition. Production supplies the bounded long-wait configuration.
fn build_router_with_session_manager<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    sessions: Arc<LocalSessionManager>,
) -> Router {
    let server_app = app.clone();
    let service = StreamableHttpService::new(
        move || Ok(PrmonitorMcp::new(server_app.clone())),
        sessions,
        StreamableHttpServerConfig::default().with_sse_keep_alive(Some(Duration::from_secs(15))),
    );
    Router::new()
        .nest_service("/mcp", service)
        .layer(RequestBodyLimitLayer::new(MAX_MCP_BODY_BYTES))
        .layer(middleware::from_fn_with_state(
            Arc::new(AuthState { app }),
            authorize::<R>,
        ))
}

async fn authorize<R: tauri::Runtime>(
    State(state): State<Arc<AuthState<R>>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // A LocalApi route can coexist with tunneled entrypoints. Refuse a public Host/Origin here so
    // MCP stays loopback-only even if a future router composition accidentally exposes the route.
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !loopback_authority(host) {
        return Err(StatusCode::FORBIDDEN);
    }
    if let Some(origin) = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        if !loopback_origin(origin) {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    let cfg = config_service::load(&state.app).map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let expected = cfg.local_api_token.trim();
    if expected.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !bool::from(expected.as_bytes().ct_eq(provided.as_bytes())) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(request).await)
}

fn loopback_authority(authority: &str) -> bool {
    let host = authority
        .strip_prefix('[')
        .and_then(|value| value.split(']').next())
        .unwrap_or_else(|| authority.split(':').next().unwrap_or(""));
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

fn loopback_origin(origin: &str) -> bool {
    url::Url::parse(origin)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::Manager;

    fn app_with_local_api_token(token: &str) -> tauri::App<tauri::test::MockRuntime> {
        let app = tauri::test::mock_app();
        let db = Database::open_in_memory().expect("open test db");
        let mut config = config_service::load_db(&db).expect("load test config");
        config.local_api_token = token.to_string();
        config_service::persist_db(&db, &config).expect("persist test config");
        app.manage(db);
        app
    }

    async fn spawn_mcp_server(
        app: &tauri::App<tauri::test::MockRuntime>,
    ) -> (u16, tauri::async_runtime::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind loopback");
        let port = listener.local_addr().expect("listener addr").port();
        let router = build_router(app.handle().clone());
        let server = tauri::async_runtime::spawn(async move {
            let _ = axum::serve(listener, router.into_make_service()).await;
        });
        (port, server)
    }

    fn mcp_request(client: &reqwest::Client, url: &str, token: &str) -> reqwest::RequestBuilder {
        client
            .post(url)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
    }

    #[test]
    fn mcp_rejects_forged_host_and_lan_origin() {
        assert!(loopback_authority("127.0.0.1:8788"));
        assert!(loopback_authority("[::1]:8788"));
        assert!(!loopback_authority("127.0.0.1.evil.example:8788"));
        assert!(!loopback_authority("192.168.1.20:8788"));
        assert!(loopback_origin("http://localhost:8788"));
        assert!(!loopback_origin("http://192.168.1.20:8788"));
    }

    #[test]
    fn session_lifetime_covers_the_longest_tool_wait() {
        let manager = human_input_session_manager();
        assert!(
            manager.session_config.keep_alive
                >= Some(Duration::from_secs(
                    human_input::MAX_WAIT_SECONDS + SESSION_CLOSE_GRACE_SECONDS
                ))
        );
    }

    #[tokio::test]
    async fn router_enforces_transport_body_limit_before_rmcp_collects() {
        let token = "mcp-test-token";
        let app = app_with_local_api_token(token);
        let (port, server) = spawn_mcp_server(&app).await;
        let client = reqwest::Client::new();
        let response = mcp_request(&client, &format!("http://127.0.0.1:{port}/mcp"), token)
            .body("x".repeat(MAX_MCP_BODY_BYTES + 1))
            .send()
            .await
            .expect("send oversized request");

        assert_eq!(response.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
        server.abort();
    }

    #[tokio::test]
    async fn router_runs_initialize_list_and_tool_error_flow() {
        let token = "mcp-flow-token";
        let app = app_with_local_api_token(token);
        let (port, server) = spawn_mcp_server(&app).await;
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/mcp");

        let initialize = mcp_request(&client, &url, token)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "prmonitor-test", "version": "1"}
                }
            }))
            .send()
            .await
            .expect("initialize request");
        assert_eq!(initialize.status(), reqwest::StatusCode::OK);
        let session = initialize
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .expect("MCP session id")
            .to_string();

        let initialized = mcp_request(&client, &url, token)
            .header("mcp-session-id", &session)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }))
            .send()
            .await
            .expect("initialized notification");
        assert!(initialized.status().is_success());

        let tools = mcp_request(&client, &url, token)
            .header("mcp-session-id", &session)
            .json(&serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
            }))
            .send()
            .await
            .expect("tools/list");
        assert_eq!(tools.status(), reqwest::StatusCode::OK);
        assert!(tools
            .text()
            .await
            .expect("tools/list body")
            .contains("ask_via_feishu"));
        // Re-list to assert DingTalk tool registration without consuming the prior body twice.
        let tools_again = mcp_request(&client, &url, token)
            .header("mcp-session-id", &session)
            .json(&serde_json::json!({
                "jsonrpc": "2.0", "id": 22, "method": "tools/list", "params": {}
            }))
            .send()
            .await
            .expect("tools/list again");
        assert!(tools_again
            .text()
            .await
            .expect("tools/list body")
            .contains("ask_via_dingtalk"));

        let invalid_call = mcp_request(&client, &url, token)
            .header("mcp-session-id", &session)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "ask_via_feishu",
                    "arguments": {
                        "purpose": "question",
                        "title": "test",
                        "message": "test",
                        "questions": [],
                        "timeoutSecs": 1
                    }
                }
            }))
            .send()
            .await
            .expect("tools/call");
        assert_eq!(invalid_call.status(), reqwest::StatusCode::OK);
        assert!(invalid_call
            .text()
            .await
            .expect("tools/call body")
            .contains("questions 必须包含 1-3 个问题"));

        let invalid_dingtalk = mcp_request(&client, &url, token)
            .header("mcp-session-id", &session)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "ask_via_dingtalk",
                    "arguments": {
                        "purpose": "question",
                        "title": "test",
                        "message": "test",
                        "questions": [],
                        "timeoutSecs": 1
                    }
                }
            }))
            .send()
            .await
            .expect("tools/call dingtalk");
        assert_eq!(invalid_dingtalk.status(), reqwest::StatusCode::OK);
        assert!(invalid_dingtalk
            .text()
            .await
            .expect("tools/call dingtalk body")
            .contains("questions 必须包含 1-3 个问题"));

        let closed = client
            .delete(&url)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header("mcp-session-id", &session)
            .send()
            .await
            .expect("close session");
        assert!(closed.status().is_success());
        server.abort();
    }

    #[tokio::test]
    async fn invalid_codex_accept_keeps_waiting_until_feishu_answers() {
        let db = Database::open_in_memory().unwrap();
        let broker = HumanInputBroker::default();
        let request = HumanInputRequest {
            id: "Q-invalid-accept".into(),
            integration_id: "feishu-main".into(),
            conversation_id: "oc_1".into(),
            purpose: "question".into(),
            title: "Choose".into(),
            message: "Pick".into(),
            questions: vec![HumanQuestion {
                id: "q1".into(),
                question: "Which?".into(),
                options: vec!["A".into(), "B".into()],
            }],
            context: serde_json::json!({}),
            status: human_input::HumanInputStatus::Pending,
            answer: None,
            answer_source: None,
            card_message_id: None,
            created_at_epoch: 10,
            expires_at_epoch: 100,
            answered_at_epoch: None,
        };
        human_input::create(&db, &request).unwrap();
        let mut receiver = broker.subscribe(&request.id);

        let wait = accept_codex_or_wait_channel(
            &db,
            &broker,
            &request,
            &mut receiver,
            Some(serde_json::json!({"answer": ""})),
            tokio::time::Instant::now() + Duration::from_secs(1),
        );
        let answer = async {
            tokio::task::yield_now().await;
            human_input::answer(
                &db,
                &broker,
                &request.id,
                &[HumanAnswer {
                    question_id: "q1".into(),
                    answer: "A".into(),
                }],
                HumanAnswerSource::Feishu,
                20,
            )
            .unwrap()
        };
        let (winner, outcome) = tokio::join!(wait, answer);

        assert_eq!(outcome, human_input::AnswerOutcome::Won);
        let winner = winner.unwrap();
        assert_eq!(winner.status, HumanInputStatus::Answered);
        assert_eq!(winner.answer_source, Some(HumanAnswerSource::Feishu));
    }

    #[tokio::test]
    async fn codex_cancel_keeps_waiting_until_feishu_answers() {
        let db = Database::open_in_memory().unwrap();
        let broker = HumanInputBroker::default();
        let request = HumanInputRequest {
            id: "Q-Cancel".into(),
            integration_id: "feishu-main".into(),
            conversation_id: "oc_1".into(),
            purpose: "question".into(),
            title: "Choose".into(),
            message: "Pick".into(),
            questions: vec![HumanQuestion {
                id: "q1".into(),
                question: "Which?".into(),
                options: vec!["A".into(), "B".into()],
            }],
            context: serde_json::json!({}),
            status: HumanInputStatus::Pending,
            answer: None,
            answer_source: None,
            card_message_id: None,
            created_at_epoch: 10,
            expires_at_epoch: 100,
            answered_at_epoch: None,
        };
        human_input::create(&db, &request).unwrap();
        let mut receiver = broker.subscribe(&request.id);

        let wait = continue_after_codex_cancel(
            &db,
            &broker,
            &request,
            &mut receiver,
            tokio::time::Instant::now() + Duration::from_secs(1),
        );
        let answer = async {
            tokio::task::yield_now().await;
            human_input::answer(
                &db,
                &broker,
                &request.id,
                &[HumanAnswer {
                    question_id: "q1".into(),
                    answer: "A".into(),
                }],
                HumanAnswerSource::Feishu,
                20,
            )
            .unwrap()
        };
        let (winner, outcome) = tokio::join!(wait, answer);

        assert_eq!(outcome, human_input::AnswerOutcome::Won);
        let winner = winner.unwrap();
        assert_eq!(winner.status, HumanInputStatus::Answered);
        assert_eq!(winner.answer_source, Some(HumanAnswerSource::Feishu));
    }

    #[tokio::test]
    async fn codex_decline_cancels_human_input_request() {
        let db = Database::open_in_memory().unwrap();
        let broker = HumanInputBroker::default();
        let request = HumanInputRequest {
            id: "Q-Decline".into(),
            integration_id: "feishu-main".into(),
            conversation_id: "oc_1".into(),
            purpose: "question".into(),
            title: "Choose".into(),
            message: "Pick".into(),
            questions: vec![HumanQuestion {
                id: "q1".into(),
                question: "Which?".into(),
                options: vec!["A".into(), "B".into()],
            }],
            context: serde_json::json!({}),
            status: HumanInputStatus::Pending,
            answer: None,
            answer_source: None,
            card_message_id: None,
            created_at_epoch: 10,
            expires_at_epoch: 100,
            answered_at_epoch: None,
        };
        human_input::create(&db, &request).unwrap();

        let cancelled = cancel_after_codex_decline(&db, &broker, &request.id).unwrap();
        assert_eq!(cancelled.status, HumanInputStatus::Cancelled);

        let outcome = human_input::answer(
            &db,
            &broker,
            &request.id,
            &[HumanAnswer {
                question_id: "q1".into(),
                answer: "A".into(),
            }],
            HumanAnswerSource::Feishu,
            20,
        )
        .unwrap();
        assert_ne!(outcome, human_input::AnswerOutcome::Won);
        assert_eq!(
            human_input::get(&db, &request.id).unwrap().unwrap().status,
            HumanInputStatus::Cancelled
        );
    }
}
