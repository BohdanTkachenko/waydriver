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
