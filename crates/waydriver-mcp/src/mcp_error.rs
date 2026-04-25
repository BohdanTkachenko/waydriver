//! Conversion from [`waydriver::Error`] to [`rmcp::ErrorData`] (`McpError`).
//!
//! `McpError` is JSON-RPC-shaped: just `{ code, message, data }`. It does
//! *not* carry an `Error::source` chain, so the chain we so carefully
//! preserved on `waydriver::Error` (D-Bus, GStreamer, IO errors hanging
//! off the top-level variant) would be lost on a naive `.to_string()`.
//!
//! [`waydriver_to_mcp`] walks the chain and joins it into the message
//! with `" | "` separators so the agent sees the full failure context.
//! It also discriminates locator-shape errors (bad selector, no match,
//! ambiguous match) into `invalid_params`, leaving infrastructure
//! failures as `internal_error`.

use rmcp::ErrorData as McpError;

/// Convert a [`waydriver::Error`] into the appropriate [`McpError`] with
/// the full source chain serialized into the message.
///
/// Provided as a free function rather than `From` because `McpError`'s
/// definition is in `rmcp` and `waydriver::Error` is in `waydriver`, so
/// neither side can carry the impl — and routing the conversion through
/// a third-party newtype just to satisfy the orphan rule would obscure
/// the call sites.
pub fn waydriver_to_mcp(e: waydriver::Error) -> McpError {
    let msg = format_chain(&e);
    if is_locator_shape(&e) {
        McpError::invalid_params(msg, None)
    } else {
        McpError::internal_error(msg, None)
    }
}

/// Walk the `Error::source` chain and join all messages with `" | "`.
///
/// Exposed at crate scope so the tool-runner can write the same rich
/// failure string into the event log that the MCP response carries.
pub(crate) fn format_chain(top: &dyn std::error::Error) -> String {
    let mut parts: Vec<String> = vec![top.to_string()];
    let mut cur = top.source();
    while let Some(src) = cur {
        parts.push(src.to_string());
        cur = src.source();
    }
    parts.join(" | ")
}

/// Errors that signal "the caller's selector / params are wrong"
/// rather than "infrastructure failed" — these map to `invalid_params`
/// (JSON-RPC code -32602) so MCP clients can surface them as user
/// input problems instead of server faults.
fn is_locator_shape(e: &waydriver::Error) -> bool {
    matches!(
        e,
        waydriver::Error::ElementNotFound { .. }
            | waydriver::Error::AmbiguousSelector { .. }
            | waydriver::Error::InvalidSelector { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locator_errors_map_to_invalid_params() {
        let err = waydriver::Error::ElementNotFound {
            xpath: "//PushButton[@name='OK']".into(),
        };
        let mcp = waydriver_to_mcp(err);
        // invalid_params is JSON-RPC code -32602.
        assert_eq!(mcp.code.0, -32602);
        assert!(mcp.message.contains("//PushButton"));
    }

    #[test]
    fn ambiguous_selector_maps_to_invalid_params() {
        let err = waydriver::Error::AmbiguousSelector {
            xpath: "//Button".into(),
            count: 3,
        };
        let mcp = waydriver_to_mcp(err);
        assert_eq!(mcp.code.0, -32602);
        assert!(mcp.message.contains("3"));
    }

    #[test]
    fn infra_errors_map_to_internal_error() {
        let err = waydriver::Error::process("dbus-launch failed");
        let mcp = waydriver_to_mcp(err);
        // internal_error is JSON-RPC code -32603.
        assert_eq!(mcp.code.0, -32603);
        assert!(mcp.message.contains("dbus-launch"));
    }

    #[test]
    fn source_chain_is_serialized_into_message() {
        let io_err = std::io::Error::other("permission denied");
        let err = waydriver::Error::screenshot_with("CreateSession", io_err);
        let mcp = waydriver_to_mcp(err);
        // The top-level message includes the operation, and the chain
        // walk picks up the io::Error's "permission denied" as the
        // source. (screenshot_with already inlines the source into
        // message via format!, so the chain entry is a duplicate —
        // that's fine; the goal is no-loss, not minimum-redundancy.)
        assert!(mcp.message.contains("CreateSession"));
        assert!(mcp.message.contains("permission denied"));
        // The " | " separator is present because the chain walk found
        // the boxed io::Error source.
        assert!(
            mcp.message.contains(" | "),
            "expected chain separator, got: {}",
            mcp.message
        );
    }

    #[test]
    fn sourceless_error_has_no_separator() {
        let err = waydriver::Error::atspi("registry unavailable");
        let mcp = waydriver_to_mcp(err);
        assert!(!mcp.message.contains(" | "));
        assert!(mcp.message.contains("registry unavailable"));
    }
}
