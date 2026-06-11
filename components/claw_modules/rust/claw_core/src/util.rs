//! Small logging helpers used by [`crate::iteration_loop`].

const LOG_SNIPPET_LEN: usize = 96;

/// Number of leading bytes to show for a log snippet.
pub fn log_snippet_len(text: &str) -> usize {
    text.len().min(LOG_SNIPPET_LEN)
}

/// `"..."` if the text was truncated, else `""`.
pub fn log_snippet_suffix(text: &str) -> &'static str {
    if text.len() > LOG_SNIPPET_LEN {
        "..."
    } else {
        ""
    }
}

/// Render a log snippet for completion logging.
pub fn truncate_for_log(text: &str) -> String {
    let mut end = log_snippet_len(text);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &text[..end], log_snippet_suffix(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_len_and_suffix() {
        let short = "hi";
        assert_eq!(log_snippet_len(short), 2);
        assert_eq!(log_snippet_suffix(short), "");
        let long = "x".repeat(LOG_SNIPPET_LEN + 10);
        assert_eq!(log_snippet_len(&long), LOG_SNIPPET_LEN);
        assert_eq!(log_snippet_suffix(&long), "...");
    }
}
