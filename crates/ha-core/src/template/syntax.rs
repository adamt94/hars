//! Jinja's wording for template syntax errors (`jinja2/parser.py`).
//!
//! minijinja finds the error; this gives the message HA would show. The
//! block structure errors people make most (an unknown or misplaced tag,
//! a block left open) are rebuilt exactly as Jinja's `_fail_ut_eof` words
//! them, from a scan of the template's `{% %}` tags. Other errors keep
//! minijinja's message in Jinja's vocabulary, which is close but not
//! always Jinja's text.

/// Jinja's message for a template minijinja couldn't parse, given
/// minijinja's `detail`.
pub(super) fn message(source: &str, detail: &str) -> String {
    // An unknown tag, or the template ending inside a block: Jinja's
    // message says which blocks are open. (The scan gives up on an
    // unclosed `{{`, whose error is about the expression.)
    let structural =
        detail.starts_with("unknown statement") || detail.starts_with("unexpected end of input");
    if structural && let Some(m) = block_error(source) {
        return m;
    }
    match detail {
        "unexpected end of input, expected end of variable block" => {
            "unexpected end of template, expected 'end of print statement'.".into()
        }
        "unexpected end of variable block" => "unexpected 'end of print statement'".into(),
        "unexpected end of block" => "unexpected 'end of statement block'".into(),
        _ => detail
            .replace("end of variable block", "'end of print statement'")
            .replace("end of block", "'end of statement block'")
            .replace("end of input", "end of template"),
    }
}

/// One open block: its tag (`_tag_stack`) and the tags that may come next
/// (`_end_token_stack`'s top).
struct Open {
    tag: &'static str,
    looking_for: &'static [&'static str],
}

/// The first unknown tag or unclosed block, worded as Jinja does.
fn block_error(source: &str) -> Option<String> {
    let mut stack: Vec<Open> = Vec::new();
    for Tag { name, assigns } in tags(source)? {
        let opened = match name {
            "if" => Some(("if", &["elif", "else", "endif"][..])),
            "for" => Some(("for", &["endfor", "else"][..])),
            "macro" => Some(("macro", &["endmacro"][..])),
            "call" => Some(("call", &["endcall"][..])),
            "filter" => Some(("filter", &["endfilter"][..])),
            "with" => Some(("with", &["endwith"][..])),
            "autoescape" => Some(("autoescape", &["endautoescape"][..])),
            "block" => Some(("block", &["endblock"][..])),
            "set" if !assigns => Some(("set", &["endset"][..])),
            _ => None,
        };
        if let Some((tag, looking_for)) = opened {
            stack.push(Open { tag, looking_for });
            continue;
        }
        let top = stack.last();
        let expected = top.is_some_and(|o| o.looking_for.contains(&name));
        match (name, top) {
            ("elif", Some(o)) if expected && o.tag == "if" => {}
            ("else", Some(o)) if expected && o.tag == "if" => {
                stack.last_mut()?.looking_for = &["endif"];
            }
            ("else", Some(o)) if expected && o.tag == "for" => {
                stack.last_mut()?.looking_for = &["endfor"];
            }
            (end, Some(_)) if expected && end.starts_with("end") => {
                stack.pop();
            }
            (
                "set" | "print" | "include" | "import" | "from" | "extends" | "do" | "break"
                | "continue",
                _,
            ) => {}
            _ => return Some(unknown_tag(Some(name), &stack)),
        }
    }
    (!stack.is_empty()).then(|| unknown_tag(None, &stack))
}

/// `Parser._fail_ut_eof`.
fn unknown_tag(name: Option<&str>, stack: &[Open]) -> String {
    let mut m = match name {
        Some(n) => format!("Encountered unknown tag '{n}'."),
        None => "Unexpected end of template.".into(),
    };
    if let Some(top) = stack.last() {
        let looking = top
            .looking_for
            .iter()
            .map(|t| format!("'{t}'"))
            .collect::<Vec<_>>()
            .join(" or ");
        let anywhere = name.is_some_and(|n| stack.iter().any(|o| o.looking_for.contains(&n)));
        if anywhere {
            m.push_str(&format!(
                " You probably made a nesting mistake. Jinja is expecting this tag, but currently looking for {looking}."
            ));
        } else {
            m.push_str(&format!(
                " Jinja was looking for the following tags: {looking}."
            ));
        }
        m.push_str(&format!(
            " The innermost block that needs to be closed is '{}'.",
            top.tag
        ));
    }
    m
}

/// A `{% name … %}` tag; `assigns` when it has a top-level `=` (a `set`
/// that isn't a block).
struct Tag<'a> {
    name: &'a str,
    assigns: bool,
}

/// The template's statement tags in order, skipping `{{ }}`, comments and
/// `{% raw %}` blocks. `None` when the template can't be scanned (an
/// unclosed delimiter, a tag without a name): the caller falls back.
fn tags(source: &str) -> Option<Vec<Tag<'_>>> {
    let b = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(at) = find(b, i, b"{") {
        match b.get(at + 1) {
            Some(b'{') => i = close(b, at + 2, b"}}")?.1,
            Some(b'#') => i = find(b, at + 2, b"#}")? + 2,
            Some(b'%') => {
                let (body, next) = close(b, at + 2, b"%}")?;
                let body = source[body].trim_start_matches(['-', '+']);
                let body = body.trim_end_matches(['-', '+']).trim();
                let len = body
                    .bytes()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == b'_')
                    .count();
                if len == 0 {
                    return None;
                }
                let name = &body[..len];
                if name == "raw" {
                    i = end_raw(source, next)?;
                    continue;
                }
                let assigns = top_level_assign(&body[len..]);
                out.push(Tag { name, assigns });
                i = next;
            }
            _ => i = at + 1,
        }
    }
    Some(out)
}

fn find(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    b.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| from + p)
}

/// The inside of a `{{ }}` or `{% %}` up to `end`, skipping string
/// literals, and the index after `end`.
fn close(b: &[u8], from: usize, end: &[u8]) -> Option<(std::ops::Range<usize>, usize)> {
    let mut i = from;
    let mut quote = None;
    while i < b.len() {
        match (quote, b[i]) {
            (Some(_), b'\\') => i += 1,
            (Some(q), c) if c == q => quote = None,
            (None, q @ (b'\'' | b'"')) => quote = Some(q),
            (None, _) if b[i..].starts_with(end) => return Some((from..i, i + end.len())),
            _ => {}
        }
        i += 1;
    }
    None
}

/// After `{% raw %}`: the index after its `{% endraw %}`.
fn end_raw(source: &str, from: usize) -> Option<usize> {
    let b = source.as_bytes();
    let mut i = from;
    loop {
        let at = find(b, i, b"{%")?;
        let (body, next) = close(b, at + 2, b"%}")?;
        if source[body].trim_matches(['-', '+']).trim() == "endraw" {
            return Some(next);
        }
        i = at + 2;
    }
}

/// A `=` outside strings that isn't part of `==`, `!=`, `<=` or `>=`.
fn top_level_assign(rest: &str) -> bool {
    let b = rest.as_bytes();
    let mut quote = None;
    let mut i = 0;
    while i < b.len() {
        match (quote, b[i]) {
            (Some(_), b'\\') => i += 1,
            (Some(q), c) if c == q => quote = None,
            (None, q @ (b'\'' | b'"')) => quote = Some(q),
            (None, b'=') => {
                let before = i.checked_sub(1).map(|j| b[j]);
                let after = b.get(i + 1).copied();
                if !matches!(before, Some(b'=' | b'!' | b'<' | b'>')) && after != Some(b'=') {
                    return true;
                }
                if after == Some(b'=') {
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}
