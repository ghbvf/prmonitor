//! Shared application error type returned by Tauri commands.

use serde::Serialize;

/// Error serialized back to the frontend from `#[tauri::command]`s.
#[derive(Debug, Clone, Serialize)]
pub struct AppError {
    pub message: String,
}

impl AppError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AppError {}

impl From<String> for AppError {
    fn from(message: String) -> Self {
        Self { message }
    }
}

/// Convenience result alias for command and service signatures.
pub type AppResult<T> = Result<T, AppError>;
