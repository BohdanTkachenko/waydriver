//! Per-concern tool groups for `UiTestServer`.
//!
//! rmcp's `#[tool_router]` attribute macro operates on a single `impl`
//! block: it collects every `#[tool]` method in that block and
//! generates a named constructor (e.g. `fn lifecycle_router()`) that
//! returns a fresh `ToolRouter<Self>`. Because `ToolRouter<S>`
//! implements `Add`/`AddAssign`/`merge`, we can have several such
//! impls — scoped by responsibility — and compose them into the
//! single router stored on the struct.
//!
//! [`UiTestServer::new`](crate::UiTestServer::new) is the composition
//! point: it sums the five routers below into the `tool_router` field
//! that the `#[tool_handler]`-generated `ServerHandler::call_tool`
//! dispatches against.

pub mod capture;
pub mod inspection;
pub mod interaction;
pub mod lifecycle;

/// Build a locator for `xpath`, applying the caller's optional per-op
/// `timeout_ms` as the element auto-wait deadline. When `None`, the locator
/// keeps the session default (`FALLBACK_DEFAULT_TIMEOUT`, 5s). Centralised
/// here so every element-driven tool surfaces the same `timeout_ms` override
/// identically instead of each re-implementing the `with_timeout` plumbing.
pub(crate) fn locate(
    session: &std::sync::Arc<waydriver::Session>,
    xpath: &str,
    timeout_ms: Option<u64>,
) -> waydriver::Locator {
    let locator = session.locate(xpath);
    match timeout_ms {
        Some(ms) => locator.with_timeout(std::time::Duration::from_millis(ms)),
        None => locator,
    }
}
