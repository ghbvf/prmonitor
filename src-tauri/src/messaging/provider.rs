//! Messaging provider seam (#1559).

use std::future::Future;
use std::pin::Pin;

use axum::http::HeaderMap;

use crate::config::service::MessagingIntegration;
use crate::error::AppResult;
use crate::model::{
    ActionExecutionResult, MessagingEvent, MessagingProviderCapability, MessagingProviderKind,
    MessagingReplyTarget, MessagingSendContent,
};

pub type ProviderFuture<'a> =
    Pin<Box<dyn Future<Output = AppResult<ActionExecutionResult>> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    UrlVerification { challenge: String },
    Event,
}

pub trait MessagingProvider: Send + Sync {
    fn kind(&self) -> MessagingProviderKind;
    fn capability(&self) -> MessagingProviderCapability;
    fn verify(
        &self,
        headers: &HeaderMap,
        raw: &[u8],
        integration: &MessagingIntegration,
    ) -> AppResult<Verification>;
    fn parse_event(
        &self,
        raw: &[u8],
        integration: &MessagingIntegration,
        now: u64,
    ) -> AppResult<MessagingEvent>;
    fn reply<'a>(
        &'a self,
        integration: &'a MessagingIntegration,
        target: &'a MessagingReplyTarget,
        text: &'a str,
    ) -> ProviderFuture<'a>;
    fn send<'a>(
        &'a self,
        integration: &'a MessagingIntegration,
        conversation_id: &'a str,
        content: &'a MessagingSendContent,
    ) -> ProviderFuture<'a>;
}
