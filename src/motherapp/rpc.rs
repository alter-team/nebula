//! The tool side of motherapp's wire protocol: JSON-RPC 2.0, one message per
//! line. Mirrors what motherapp's own tool SDK does (`packages/tool-sdk` and
//! `packages/protocol/src/rpc.ts` there), so the core cannot tell the difference.

use serde_json::{Value, json};

/// Action failed (the SDK's code for a thrown error).
pub const ACTION_FAILED: i64 = -32000;
pub const INVALID_PARAMS: i64 = -32602;
pub const METHOD_NOT_FOUND: i64 = -32601;

#[derive(Debug, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
        }
    }
    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(ACTION_FAILED, message)
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }
}

#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// Must be answered with the same `id`.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    /// A reply to a request; this side never sends one, so it is dropped.
    Response,
    /// Not a JSON-RPC 2.0 message; reported on stderr and otherwise ignored.
    Invalid(&'static str),
}

/// Decode one input line. A batch (JSON array) yields one entry per element.
pub fn parse_line(line: &str) -> Vec<Incoming> {
    match serde_json::from_str::<Value>(line) {
        Ok(v) => {
            let mut out = Vec::new();
            classify(v, &mut out);
            out
        }
        Err(_) => vec![Incoming::Invalid("not JSON")],
    }
}

fn classify(msg: Value, out: &mut Vec<Incoming>) {
    let mut m = match msg {
        Value::Array(items) => {
            for item in items {
                classify(item, out);
            }
            return;
        }
        Value::Object(m) if m.get("jsonrpc") == Some(&json!("2.0")) => m,
        _ => {
            out.push(Incoming::Invalid("not a JSON-RPC 2.0 message"));
            return;
        }
    };
    if let Some(Value::String(method)) = m.get("method") {
        let method = method.clone();
        let params = m.remove("params").unwrap_or(Value::Null);
        // Only a number or a string is a request id; anything else makes it a notification.
        match m.remove("id") {
            Some(id @ (Value::Number(_) | Value::String(_))) => out.push(Incoming::Request {
                id,
                method,
                params,
            }),
            _ => out.push(Incoming::Notification { method, params }),
        }
    } else if m.contains_key("result") || m.contains_key("error") {
        out.push(Incoming::Response);
    } else {
        out.push(Incoming::Invalid("neither request nor response"));
    }
}

/// serde_json never emits a raw newline, so every message is one line.
pub fn result_line(id: &Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

pub fn error_line(id: &Value, e: &RpcError) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": e.code, "message": e.message } })
        .to_string()
}

pub fn notify_line(method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_notifications() {
        assert_eq!(
            parse_line(r#"{"jsonrpc":"2.0","id":1,"method":"describe"}"#),
            vec![Incoming::Request {
                id: json!(1),
                method: "describe".into(),
                params: Value::Null
            }]
        );
        assert_eq!(
            parse_line(r#"{"jsonrpc":"2.0","id":"a","method":"action","params":{"name":"x"}}"#),
            vec![Incoming::Request {
                id: json!("a"),
                method: "action".into(),
                params: json!({"name": "x"})
            }]
        );
        assert_eq!(
            parse_line(r#"{"jsonrpc":"2.0","method":"watch","params":{"watching":true}}"#),
            vec![Incoming::Notification {
                method: "watch".into(),
                params: json!({"watching": true})
            }]
        );
        // an id that is neither number nor string: a notification, never answered
        assert_eq!(
            parse_line(r#"{"jsonrpc":"2.0","id":null,"method":"x"}"#),
            vec![Incoming::Notification {
                method: "x".into(),
                params: Value::Null
            }]
        );
    }

    #[test]
    fn invalid_input_and_batches() {
        assert_eq!(parse_line("nonsense"), vec![Incoming::Invalid("not JSON")]);
        for bad in [r#"{"id":1,"method":"x"}"#, "3", r#""s""#, "null"] {
            assert_eq!(
                parse_line(bad),
                vec![Incoming::Invalid("not a JSON-RPC 2.0 message")],
                "{bad}"
            );
        }
        assert_eq!(
            parse_line(r#"{"jsonrpc":"2.0","id":1}"#),
            vec![Incoming::Invalid("neither request nor response")]
        );
        assert_eq!(
            parse_line(r#"{"jsonrpc":"2.0","id":1,"result":null}"#),
            vec![Incoming::Response]
        );
        let batch =
            parse_line(r#"[{"jsonrpc":"2.0","id":1,"method":"a"},{"jsonrpc":"2.0","method":"b"}]"#);
        assert_eq!(batch.len(), 2);
        assert!(matches!(batch[0], Incoming::Request { .. }));
        assert!(matches!(batch[1], Incoming::Notification { .. }));
    }

    #[test]
    fn output_is_one_line_each() {
        let r = result_line(&json!(7), json!({"text": "a\nb\u{2028}c"}));
        assert!(!r.contains('\n'));
        assert_eq!(
            serde_json::from_str::<Value>(&r).unwrap(),
            json!({"jsonrpc": "2.0", "id": 7, "result": {"text": "a\nb\u{2028}c"}})
        );
        let e = error_line(&json!("x"), &RpcError::new(METHOD_NOT_FOUND, "nope"));
        assert_eq!(
            serde_json::from_str::<Value>(&e).unwrap(),
            json!({"jsonrpc": "2.0", "id": "x", "error": {"code": -32601, "message": "nope"}})
        );
        let n = notify_line("notify", json!({"kind": "finished", "title": "t"}));
        assert_eq!(
            serde_json::from_str::<Value>(&n).unwrap(),
            json!({"jsonrpc": "2.0", "method": "notify", "params": {"kind": "finished", "title": "t"}})
        );
    }
}
