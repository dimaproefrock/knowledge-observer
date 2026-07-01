//! Pure tail / watermark / strip helpers for the observer.
//! Pure, no cross-module deps.
//!
//! Everything here must be pure and unit-testable; the I/O shell lives elsewhere.

use serde_json::Value;

/// Return the complete '\n'-terminated lines in `buf` starting at byte offset
/// `watermark`, plus the new watermark (offset just past the last complete line
/// consumed). A trailing partial line (no '\n') is NOT consumed — watermark stops
/// before it so it is re-read once complete. If watermark >= buf.len(), returns
/// (empty, watermark). Lines are returned WITHOUT the trailing '\n'.
pub fn new_lines(buf: &[u8], watermark: u64) -> (Vec<String>, u64) {
    let start = watermark as usize;
    if start >= buf.len() {
        return (Vec::new(), watermark);
    }

    let mut lines = Vec::new();
    // `consumed` is the byte offset (absolute) just past the last '\n' we consumed.
    let mut consumed = start;
    let mut line_start = start;

    let mut i = start;
    while i < buf.len() {
        if buf[i] == b'\n' {
            // Complete line: bytes [line_start, i).
            let mut slice = &buf[line_start..i];
            // Trim a trailing '\r' for safety on Windows-written files.
            if let Some((&b'\r', rest)) = slice.split_last() {
                slice = rest;
            }
            lines.push(String::from_utf8_lossy(slice).into_owned());
            consumed = i + 1;
            line_start = i + 1;
        }
        i += 1;
    }

    (lines, consumed as u64)
}

/// Watermark reconciliation across transcript rotation/mutation (/clear, /compact,
/// fork, rewind). If the owned JSONL stem changed -> 0 (fresh file, reprocess from
/// start). Else if the file shrank (file_len < prev_watermark) -> clamp to file_len.
/// Else keep prev_watermark.
pub fn reconcile_watermark(
    prev_stem: &str,
    prev_watermark: u64,
    file_len: u64,
    current_stem: &str,
) -> u64 {
    if prev_stem != current_stem {
        0
    } else if file_len < prev_watermark {
        file_len
    } else {
        prev_watermark
    }
}

/// Marker prefixing observer-injected hint context (the `[Knowledge] …` UserPromptSubmit
/// hints + index-delta lines built in [`crate::observer`]). Injected `additionalContext`
/// is delivered via `hookSpecificOutput` and is NOT persisted to the JSONL transcript, so
/// this filter is a **defensive safety-net**: should such text ever leak into the tailed
/// transcript, `is_injected` drops it so the observer never re-records its own output.
pub const INJECTED_MARKER: &str = "[Knowledge]";

/// Returns false when `sentinel` is empty (no injection yet). When a non-empty
/// sentinel is given, returns true if the line contains it. (The injected-context
/// filter so the watcher can skip its own injections.)
pub fn is_injected(line: &str, sentinel: &str) -> bool {
    if sentinel.is_empty() {
        return false;
    }
    line.contains(sentinel)
}

/// Default cap for tool_result content length before stripping.
pub const MAX_TOOL_RESULT_LEN: usize = 2000;

/// Given ONE JSONL transcript line (a JSON object string), return the line with any
/// large `tool_result` content shortened to `max_len` chars + a "…[gekürzt]" marker
/// (file dumps / diffs are noise for extraction — rely on the assistant's own prose).
/// If the line is not valid JSON, or has no tool_result, return it unchanged.
pub fn strip_tool_results(line: &str, max_len: usize) -> String {
    let mut value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return line.to_string(),
    };

    let mut changed = false;
    strip_in_value(&mut value, max_len, &mut changed);

    if !changed {
        return line.to_string();
    }

    serde_json::to_string(&value).unwrap_or_else(|_| line.to_string())
}

/// Walk a JSON value looking for `tool_result` blocks (objects with
/// `"type":"tool_result"`) and shorten their `content`. Returns whether anything
/// was changed via `changed`.
fn strip_in_value(value: &mut Value, max_len: usize, changed: &mut bool) {
    match value {
        Value::Object(map) => {
            let is_tool_result = map
                .get("type")
                .and_then(|t| t.as_str())
                .map(|t| t == "tool_result")
                .unwrap_or(false);

            if is_tool_result {
                if let Some(content) = map.get_mut("content") {
                    shorten_content(content, max_len, changed);
                }
            }

            // Recurse into all nested values regardless (content arrays, message
            // objects, etc.) so deeply nested tool_results are also caught.
            for (_k, v) in map.iter_mut() {
                strip_in_value(v, max_len, changed);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                strip_in_value(v, max_len, changed);
            }
        }
        _ => {}
    }
}

/// Shorten a tool_result `content` field, which is either a string or an array of
/// `{"type":"text","text":...}` blocks.
fn shorten_content(content: &mut Value, max_len: usize, changed: &mut bool) {
    match content {
        Value::String(s) => {
            if let Some(short) = truncate_chars(s, max_len) {
                *s = short;
                *changed = true;
            }
        }
        Value::Array(arr) => {
            for block in arr.iter_mut() {
                if let Value::Object(map) = block {
                    if let Some(Value::String(s)) = map.get_mut("text") {
                        if let Some(short) = truncate_chars(s, max_len) {
                            *s = short;
                            *changed = true;
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// If `s` is longer than `max_len` chars, return Some(truncated + marker); else None.
fn truncate_chars(s: &str, max_len: usize) -> Option<String> {
    // Count chars without allocating unless we must truncate.
    if s.chars().count() <= max_len {
        return None;
    }
    let mut out: String = s.chars().take(max_len).collect();
    out.push_str("…[gekürzt]");
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    // ---- new_lines ----

    #[test]
    fn new_lines_three_full_lines() {
        let buf = b"line one\nline two\nline three\n";
        let (lines, wm) = new_lines(buf, 0);
        assert_eq!(lines, vec!["line one", "line two", "line three"]);
        assert_eq!(wm, buf.len() as u64);
    }

    #[test]
    fn new_lines_trailing_partial_not_consumed() {
        let buf = b"complete\npartial without newline";
        let (lines, wm) = new_lines(buf, 0);
        assert_eq!(lines, vec!["complete"]);
        // Watermark stops right after the first '\n'.
        assert_eq!(wm, "complete\n".len() as u64);
    }

    #[test]
    fn new_lines_resumes_after_partial_completes() {
        let buf1 = b"complete\npartial";
        let (_lines1, wm1) = new_lines(buf1, 0);
        // Same buffer grew: the partial line now has its newline.
        let buf2 = b"complete\npartial now complete\n";
        let (lines2, wm2) = new_lines(buf2, wm1);
        assert_eq!(lines2, vec!["partial now complete"]);
        assert_eq!(wm2, buf2.len() as u64);
    }

    #[test]
    fn new_lines_watermark_at_or_past_end() {
        let buf = b"only line\n";
        let (lines, wm) = new_lines(buf, buf.len() as u64);
        assert!(lines.is_empty());
        assert_eq!(wm, buf.len() as u64);

        let (lines2, wm2) = new_lines(buf, (buf.len() + 100) as u64);
        assert!(lines2.is_empty());
        assert_eq!(wm2, (buf.len() + 100) as u64);
    }

    #[test]
    fn new_lines_multibyte_utf8_survives() {
        // "über — Größe 文字\n" contains multi-byte chars; slicing must be byte-safe.
        let s = "über — Größe 文字\nzweite Zeile\n";
        let buf = s.as_bytes();
        let (lines, wm) = new_lines(buf, 0);
        assert_eq!(lines, vec!["über — Größe 文字", "zweite Zeile"]);
        assert_eq!(wm, buf.len() as u64);
    }

    #[test]
    fn new_lines_trims_trailing_cr() {
        let buf = b"windows line\r\nunix line\n";
        let (lines, wm) = new_lines(buf, 0);
        assert_eq!(lines, vec!["windows line", "unix line"]);
        assert_eq!(wm, buf.len() as u64);
    }

    // ---- reconcile_watermark ----

    #[test]
    fn reconcile_stem_changed_resets_to_zero() {
        assert_eq!(reconcile_watermark("old", 500, 1200, "new"), 0);
    }

    #[test]
    fn reconcile_same_stem_shrank_clamps_to_file_len() {
        assert_eq!(reconcile_watermark("same", 500, 120, "same"), 120);
    }

    #[test]
    fn reconcile_same_stem_grew_or_equal_keeps_prev() {
        assert_eq!(reconcile_watermark("same", 500, 900, "same"), 500);
        assert_eq!(reconcile_watermark("same", 500, 500, "same"), 500);
    }

    // ---- is_injected ----

    #[test]
    fn is_injected_empty_sentinel_is_false() {
        assert!(!is_injected("anything at all", ""));
    }

    #[test]
    fn is_injected_matching_sentinel_is_true() {
        assert!(is_injected("prefix OBS-INJECT suffix", "OBS-INJECT"));
    }

    #[test]
    fn is_injected_non_matching_is_false() {
        assert!(!is_injected("ordinary line", "OBS-INJECT"));
    }

    #[test]
    fn is_injected_knowledge_marker_matches_hint_lines() {
        // The armed marker filters observer-injected hint/index lines by prefix.
        assert!(is_injected("[Knowledge] Index updated: 2 new node(s)", INJECTED_MARKER));
        assert!(is_injected("prefix [Knowledge] relevance nudge", INJECTED_MARKER));
        assert!(!is_injected("an ordinary user turn", INJECTED_MARKER));
    }

    // ---- strip_tool_results ----

    fn content_string(line: &str) -> Option<String> {
        let v: Value = serde_json::from_str(line).ok()?;
        let arr = v.get("message")?.get("content")?.as_array()?;
        for b in arr {
            if b.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                return b.get("content").and_then(|c| c.as_str()).map(String::from);
            }
        }
        None
    }

    #[test]
    fn strip_long_tool_result_string_truncated_with_marker() {
        let big = "X".repeat(5000);
        let line = format!(
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"T","content":"{big}"}}]}}}}"#
        );
        let out = strip_tool_results(&line, MAX_TOOL_RESULT_LEN);
        let c = content_string(&out).expect("content present");
        assert!(c.ends_with("…[gekürzt]"));
        // 2000 chars of payload + the marker.
        assert_eq!(c.chars().count(), MAX_TOOL_RESULT_LEN + "…[gekürzt]".chars().count());
        assert!(c.starts_with("XXXX"));
    }

    #[test]
    fn strip_short_tool_result_unchanged() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"T","content":"short result"}]}}"#;
        let out = strip_tool_results(line, MAX_TOOL_RESULT_LEN);
        let c = content_string(&out).unwrap();
        assert_eq!(c, "short result");
        assert!(!c.contains("[gekürzt]"));
    }

    #[test]
    fn strip_content_as_array_truncated() {
        let big = "Y".repeat(5000);
        let line = format!(
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"T","content":[{{"type":"text","text":"{big}"}}]}}]}}}}"#
        );
        let out = strip_tool_results(&line, MAX_TOOL_RESULT_LEN);
        let v: Value = serde_json::from_str(&out).unwrap();
        let text = v["message"]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(text.ends_with("…[gekürzt]"));
        assert_eq!(
            text.chars().count(),
            MAX_TOOL_RESULT_LEN + "…[gekürzt]".chars().count()
        );
    }

    #[test]
    fn strip_non_json_returned_verbatim() {
        let line = "this is not json {{{ broken";
        let out = strip_tool_results(line, MAX_TOOL_RESULT_LEN);
        assert_eq!(out, line);
    }

    #[test]
    fn strip_no_tool_result_unchanged() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"just prose"}]}}"#;
        let out = strip_tool_results(line, MAX_TOOL_RESULT_LEN);
        // Returned unchanged (byte-for-byte) because nothing was stripped.
        assert_eq!(out, line);
    }

    #[test]
    fn strip_respects_custom_max_len() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"T","content":"abcdefghij"}]}}"#;
        let out = strip_tool_results(line, 4);
        let c = content_string(&out).unwrap();
        assert_eq!(c, "abcd…[gekürzt]");
    }
}
