//! Read-only accessibility-tree inspection: `dump_tree`, `query`, `read_text`.
//!
//! Every tool here is a thin dispatcher to `run_action`. The bodies
//! only appear to add ceremony because the `#[tool(description = ...)]`
//! attribute has to carry the full prose for MCP clients to show the
//! user.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router, ErrorData as McpError};

use crate::params::{QueryParams, ReadTextParams, SessionIdParams};
use crate::report::render_matches;
use crate::UiTestServer;

#[tool_router(router = inspection_router, vis = "pub(crate)")]
impl UiTestServer {
    #[tool(
        description = "Dump the accessibility tree of the application UI as XML. Use this to \
                       discover selector-ready role names, attributes, and element hierarchy \
                       before writing XPath queries for `query` or `click`."
    )]
    pub(crate) async fn dump_tree(
        &self,
        Parameters(params): Parameters<SessionIdParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_action(
            &params.session_id,
            "dump_tree",
            serde_json::json!({}),
            |s| async move { s.dump_tree().await },
        )
        .await
    }

    #[tool(
        description = "Query the accessibility tree with an XPath selector. Returns a JSON \
                       array of matches; each element carries a pinned `xpath` that can be \
                       passed back to `click` / `set_text` / `read_text` to target that \
                       specific ordinal match. Names are not unique, so prefer more specific \
                       selectors (role + attribute) over pure name matches."
    )]
    pub(crate) async fn query(
        &self,
        Parameters(params): Parameters<QueryParams>,
    ) -> Result<CallToolResult, McpError> {
        let xpath = params.xpath.clone();
        self.run_action(
            &params.session_id,
            "query",
            serde_json::json!({ "xpath": params.xpath }),
            |s| async move {
                let matches = s.locate(&xpath).inspect_all().await?;
                // serde_json failure on a value we just constructed is
                // essentially impossible, but a typed error is cheaper
                // than a panic — wrap it as an infra failure.
                serde_json::to_string_pretty(&render_matches(&xpath, &matches))
                    .map_err(|e| waydriver::Error::process_with("serialize query result", e))
            },
        )
        .await
    }

    #[tool(
        description = "Read the text contents of an element selected by XPath. Target must \
                       implement the Text AT-SPI interface."
    )]
    pub(crate) async fn read_text(
        &self,
        Parameters(params): Parameters<ReadTextParams>,
    ) -> Result<CallToolResult, McpError> {
        let xpath = params.xpath.clone();
        self.run_action(
            &params.session_id,
            "read_text",
            serde_json::json!({ "xpath": params.xpath }),
            |s| async move { s.locate(&xpath).text().await },
        )
        .await
    }
}
