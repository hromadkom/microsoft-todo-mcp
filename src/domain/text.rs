//! HTML → text, dependency-free (m2 §7), plus char-boundary-safe truncation.

/// Drop `<script>`/`<style>` spans; `<br>`, `</p>`, `</div>`, `</li>`, `<tr`
/// → newline; strip tags; decode the six named entities plus numeric;
/// collapse whitespace.
pub fn html_to_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Skip whole script/style elements.
            for tag in ["script", "style"] {
                let open = format!("<{tag}");
                if lower[i..].starts_with(&open) {
                    let close = format!("</{tag}>");
                    match lower[i..].find(&close) {
                        Some(off) => {
                            i += off + close.len();
                        }
                        None => i = bytes.len(),
                    }
                    continue;
                }
            }
            if i >= bytes.len() {
                break;
            }
            let end = match html[i..].find('>') {
                Some(e) => i + e + 1,
                None => bytes.len(),
            };
            let tag = &lower[i..end];
            let name: String = tag
                .trim_start_matches('<')
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            let closing = tag.starts_with("</");
            if name == "br"
                || (closing
                    && matches!(
                        name.as_str(),
                        "p" | "div" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "tr"
                    ))
                || (!closing && name == "tr")
            {
                out.push('\n');
            } else if !closing && matches!(name.as_str(), "td" | "th") {
                out.push(' ');
            }
            i = end;
            continue;
        }
        if bytes[i] == b'&'
            && let Some((text, len)) = decode_entity(&html[i..])
        {
            out.push_str(&text);
            i += len;
            continue;
        }
        // Push one UTF-8 char.
        let ch = html[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    collapse_whitespace(&out)
}

fn decode_entity(s: &str) -> Option<(String, usize)> {
    let semi = s.find(';')?;
    if semi > 12 {
        return None;
    }
    let body = &s[1..semi];
    let text = match body {
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "quot" => "\"".to_string(),
        "apos" => "'".to_string(),
        "nbsp" => " ".to_string(),
        _ if body.starts_with("#x") || body.starts_with("#X") => {
            let n = u32::from_str_radix(&body[2..], 16).ok()?;
            char::from_u32(n)?.to_string()
        }
        _ if body.starts_with('#') => {
            let n = body[1..].parse::<u32>().ok()?;
            char::from_u32(n)?.to_string()
        }
        _ => return None,
    };
    Some((text, semi + 1))
}

/// Runs of spaces/tabs → one space; runs of newlines (with surrounding
/// spaces) → one newline; trimmed.
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    let mut pending_newline = false;
    for c in s.chars() {
        match c {
            '\n' | '\r' => pending_newline = true,
            c if c.is_whitespace() => pending_space = true,
            c => {
                if pending_newline {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                } else if pending_space && !out.is_empty() {
                    out.push(' ');
                }
                pending_newline = false;
                pending_space = false;
                out.push(c);
            }
        }
    }
    out
}

/// Byte-bounded, char-boundary-safe prefix. Returns `(text, truncated)`.
pub fn truncate_bytes(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// Minimal HTML escaping for outbound `body.content`.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("<br>"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_and_decodes_entities() {
        let html = "<html><body><style>p{}</style><p>Buy <b>milk</b> &amp; eggs</p><script>x()</script><div>Then&nbsp;rest</div><ul><li>one</li><li>two</li></ul>&#169; &#x41;</body></html>";
        assert_eq!(
            html_to_text(html),
            "Buy milk & eggs\nThen rest\none\ntwo\n© A"
        );
    }

    #[test]
    fn whitespace_collapses() {
        assert_eq!(html_to_text("  a   b \n\n  c  "), "a b\nc");
        assert_eq!(html_to_text("<br><br>x<br>"), "x");
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let (t, cut) = truncate_bytes("héllo", 2);
        assert_eq!(t, "h");
        assert!(cut);
        let (t, cut) = truncate_bytes("abc", 10);
        assert_eq!(t, "abc");
        assert!(!cut);
    }

    #[test]
    fn escape_round_trips_through_html_to_text() {
        let s = "a < b & c > d\nline";
        assert_eq!(html_to_text(&escape_html(s)), "a < b & c > d\nline");
    }
}
