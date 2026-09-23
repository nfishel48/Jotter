//! Last-resort redaction of filesystem paths in outbound events.
//!
//! Everything the app sends deliberately is built from `&'static str` and
//! numbers, so none of it can carry a path. This exists for the events the app
//! does *not* build: panic `$exception` payloads, whose message and stack frames
//! come from `std` and from whichever crate panicked. `.expect()` on an
//! `io::Error` prints the path it failed on, and on this platform that path
//! starts with the user's home directory — which is to say, their name.
//!
//! Applied in `before_send`, so it covers every event regardless of origin.

use serde_json::Value;

/// Rewrite `home` to `~` everywhere it appears in `value`.
///
/// Returns `None` when nothing matched, so the caller can skip re-inserting the
/// property — the common case by far.
pub fn redacted(value: &Value, home: &str) -> Option<Value> {
    if home.is_empty() || home == "/" {
        return None;
    }

    match value {
        Value::String(s) => s
            .contains(home)
            .then(|| Value::String(s.replace(home, "~"))),
        Value::Array(items) => {
            let mut changed = false;
            let rewritten: Vec<Value> = items
                .iter()
                .map(|item| match redacted(item, home) {
                    Some(new) => {
                        changed = true;
                        new
                    }
                    None => item.clone(),
                })
                .collect();
            changed.then_some(Value::Array(rewritten))
        }
        Value::Object(fields) => {
            let mut changed = false;
            let rewritten: serde_json::Map<String, Value> = fields
                .iter()
                .map(|(key, val)| match redacted(val, home) {
                    Some(new) => {
                        changed = true;
                        (key.clone(), new)
                    }
                    None => (key.clone(), val.clone()),
                })
                .collect();
            changed.then_some(Value::Object(rewritten))
        }
        _ => None,
    }
}

/// The home directory to redact, as a string.
pub fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rewrites_home_in_a_plain_string() {
        let value = json!("failed to open /Users/nfishel/jotter/assets/icon.png");
        let out = redacted(&value, "/Users/nfishel").unwrap();
        assert_eq!(out, json!("failed to open ~/jotter/assets/icon.png"));
    }

    #[test]
    fn rewrites_nested_stack_frames() {
        // Shaped like a posthog `$exception_list` payload.
        let value = json!({
            "$exception_list": [{
                "type": "panic",
                "value": "failed to open recording: /Users/nfishel/Documents/Jotter/meta.json",
                "stacktrace": {
                    "frames": [
                        { "filename": "/Users/nfishel/jotter/crates/jotter/src/audio/capture.rs", "lineno": 90 },
                        { "filename": "/rustc/deadbeef/library/std/src/panic.rs", "lineno": 1 }
                    ]
                }
            }]
        });

        let out = redacted(&value, "/Users/nfishel").unwrap();
        let rendered = out.to_string();
        assert!(!rendered.contains("nfishel"), "home survived: {rendered}");
        assert!(rendered.contains("~/jotter/crates/jotter/src/audio/capture.rs"));
        // Non-home paths are untouched: they carry no identity and the frames
        // are useless without them.
        assert!(rendered.contains("/rustc/deadbeef/library/std/src/panic.rs"));
        assert!(rendered.contains("\"lineno\":90"));
    }

    #[test]
    fn returns_none_when_nothing_matches() {
        let value = json!({ "recording_failed": "no_input_device", "count": 3 });
        assert!(redacted(&value, "/Users/nfishel").is_none());
    }

    #[test]
    fn leaves_non_strings_alone() {
        assert!(redacted(&json!(42), "/Users/nfishel").is_none());
        assert!(redacted(&json!(true), "/Users/nfishel").is_none());
        assert!(redacted(&Value::Null, "/Users/nfishel").is_none());
    }

    #[test]
    fn degenerate_home_values_are_ignored() {
        // An empty or root HOME would otherwise rewrite every "/" in every
        // string, mangling stack traces for no benefit.
        let value = json!("/usr/lib/libSystem.dylib");
        assert!(redacted(&value, "").is_none());
        assert!(redacted(&value, "/").is_none());
    }

    #[test]
    fn handles_linux_style_home() {
        let value = json!("/home/nfishel/.config/jotter/settings.json");
        let out = redacted(&value, "/home/nfishel").unwrap();
        assert_eq!(out, json!("~/.config/jotter/settings.json"));
    }
}
