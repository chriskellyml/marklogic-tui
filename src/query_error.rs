//! Readable rendering of MarkLogic errors embedded in HTTP error messages.

use serde_json::Value;

pub(crate) fn format_message(text: &str) -> String {
    if !text.starts_with("Query error") {
        return text.to_string();
    }

    // The JSON is embedded in a contextual HTTP error, and may be followed by
    // further causes. Decode one JSON value rather than requiring a JSON-only body.
    let response = text.match_indices('{').find_map(|(offset, _)| {
        serde_json::Deserializer::from_str(&text[offset..])
            .into_iter::<Value>()
            .next()
            .and_then(Result::ok)
            .and_then(|value| value.get("errorResponse").cloned())
    });
    let Some(response) = response else {
        return text.to_string();
    };

    let code = response["messageCode"].as_str().unwrap_or("MarkLogic");
    let message = response["message"].as_str().unwrap_or("Query failed");
    let detail = message
        .strip_prefix(code)
        .and_then(|rest| rest.strip_prefix(": "))
        .unwrap_or(message);
    let mut output = format!("Query error: {code}\n\n{detail}");

    if let Some(stack) = response["stackTrace"].as_str().filter(|s| !s.is_empty()) {
        // MarkLogic repeats the complete message at the start of its stack.
        let stack = stack.strip_prefix(message).unwrap_or(stack).trim();
        if !stack.is_empty() {
            output.push_str("\n\nStack trace:\n");
            output.push_str(stack);
        }
    }

    if let Some(status) = response["statusCode"].as_u64() {
        output.push_str(&format!("\n\nHTTP {status}"));
        if let Some(description) = response["status"].as_str() {
            output.push_str(&format!(" — {description}"));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_embedded_json_and_removes_duplicate_message_and_causes() {
        let message = "XDMP-UNEXPECTED: (err:XPST0003) Unexpected token syntax error";
        let json = serde_json::json!({"errorResponse": {
            "statusCode": 500, "status": "Internal Server Error",
            "messageCode": "XDMP-UNEXPECTED", "message": message,
            "stackTrace": format!("{message}\nin /eval, at 3:8 [1.0-ml]")
        }});
        let text = format!(
            "Query error: Failed to execute XQuery: POST /v1/eval: {json}\nCaused by: POST /v1/eval: {json}"
        );
        assert_eq!(
            format_message(&text),
            "Query error: XDMP-UNEXPECTED\n\n(err:XPST0003) Unexpected token syntax error\n\nStack trace:\nin /eval, at 3:8 [1.0-ml]\n\nHTTP 500 — Internal Server Error"
        );
    }

    #[test]
    fn preserves_errors_without_valid_marklogic_json() {
        for text in [
            "Running query...",
            "Query error: connection refused",
            "Query error: {invalid}",
        ] {
            assert_eq!(format_message(text), text);
        }
    }

    #[test]
    fn handles_response_without_stack_or_status() {
        assert_eq!(
            format_message(
                "Query error: {\"errorResponse\":{\"messageCode\":\"ERR\",\"message\":\"ERR: bad query\"}}"
            ),
            "Query error: ERR\n\nbad query"
        );
    }
}
