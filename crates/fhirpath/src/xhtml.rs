//! The check behind FHIR's `htmlChecks()`, which the narrative invariants `txt-1` and `txt-2` use.
//!
//! A narrative is one XHTML `div` that holds only the basic formatting elements FHIR allows,
//! carries no script (an event handler, a `javascript:` link), and has some text in it. This is a
//! small scanner for that, not an HTML parser: it reads tags, attributes and text, and says
//! whether the rules are kept.

/// The elements a narrative may use.
const ALLOWED: &[&str] = &[
    "a",
    "abbr",
    "acronym",
    "b",
    "big",
    "blockquote",
    "br",
    "caption",
    "cite",
    "code",
    "col",
    "colgroup",
    "dd",
    "dfn",
    "div",
    "dl",
    "dt",
    "em",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "li",
    "ol",
    "p",
    "pre",
    "q",
    "samp",
    "small",
    "span",
    "strong",
    "sub",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "tt",
    "u",
    "ul",
    "var",
];

/// Elements that never hold content, so they are not waiting for a closing tag.
const VOID: &[&str] = &["br", "hr", "img", "col"];

const XHTML_NAMESPACE: &str = "http://www.w3.org/1999/xhtml";

/// Whether `text` is a narrative the rules allow: well formed, a single `div` in the XHTML
/// namespace at the root, only allowed elements, no script, and some non-whitespace text.
pub fn html_checks(text: &str) -> bool {
    let mut rest = text.trim();
    let mut open: Vec<String> = Vec::new();
    let mut roots = 0usize;
    let mut has_text = false;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("<!--") {
            let Some(end) = after.find("-->") else {
                return false;
            };
            rest = &after[end + 3..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('<') {
            let Some(end) = after.find('>') else {
                return false;
            };
            let tag = &after[..end];
            rest = &after[end + 1..];
            if let Some(name) = tag.strip_prefix('/') {
                if open.pop().as_deref() != Some(name.trim().to_ascii_lowercase().as_str()) {
                    return false;
                }
                continue;
            }
            if !open_tag(tag, &mut open, &mut roots) {
                return false;
            }
            continue;
        }
        let end = rest.find('<').unwrap_or(rest.len());
        let chunk = &rest[..end];
        rest = &rest[end..];
        if !chunk.trim().is_empty() {
            // Text outside the root element is not part of the narrative.
            if open.is_empty() {
                return false;
            }
            has_text = true;
        }
    }
    roots == 1 && open.is_empty() && has_text
}

/// Check one opening (or self-closing) tag and note it as open. `false` when it breaks a rule.
fn open_tag(tag: &str, open: &mut Vec<String>, roots: &mut usize) -> bool {
    let self_closing = tag.ends_with('/');
    let tag = tag.trim_end_matches('/').trim();
    let (name, attributes) = match tag.find(char::is_whitespace) {
        Some(at) => (&tag[..at], &tag[at..]),
        None => (tag, ""),
    };
    let name = name.to_ascii_lowercase();
    if !ALLOWED.contains(&name.as_str()) {
        return false;
    }
    let Some(attributes) = attributes_of(attributes) else {
        return false;
    };
    if open.is_empty() {
        let namespaced = attributes
            .iter()
            .any(|(key, value)| key == "xmlns" && value == XHTML_NAMESPACE);
        if name != "div" || *roots > 0 || !namespaced {
            return false;
        }
        *roots += 1;
    }
    for (key, value) in &attributes {
        if key.starts_with("on") {
            return false;
        }
        if matches!(key.as_str(), "href" | "src")
            && value
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("javascript:")
        {
            return false;
        }
    }
    if !self_closing && !VOID.contains(&name.as_str()) {
        open.push(name);
    }
    true
}

/// The `name="value"` pairs of a tag, names in lower case. `None` when one is not quoted, which
/// XHTML requires.
fn attributes_of(text: &str) -> Option<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let equals = rest.find('=')?;
        let key = rest[..equals].trim().to_ascii_lowercase();
        if key.is_empty() || key.contains(char::is_whitespace) {
            return None;
        }
        let after = rest[equals + 1..].trim_start();
        let quote = after
            .chars()
            .next()
            .filter(|quote| matches!(quote, '"' | '\''))?;
        let body = &after[1..];
        let close = body.find(quote)?;
        pairs.push((key, body[..close].to_string()));
        rest = body[close + 1..].trim_start();
    }
    Some(pairs)
}

#[cfg(test)]
mod tests {
    use super::html_checks;

    fn div(inner: &str) -> String {
        format!("<div xmlns=\"http://www.w3.org/1999/xhtml\">{inner}</div>")
    }

    #[test]
    fn a_plain_narrative_passes() {
        assert!(html_checks(&div("Allergies: none known")));
        assert!(html_checks(&div(
            "<p>Heart <b>rate</b></p><table><tr><td>60</td></tr></table><br/><hr/>"
        )));
        assert!(html_checks(&div(
            "<a href=\"https://example.org/a\">a &amp; b</a>"
        )));
        assert!(html_checks(&format!("  \n{}\n", div("<!-- note -->text"))));
    }

    #[test]
    fn a_narrative_needs_a_div_in_the_xhtml_namespace_at_its_root() {
        assert!(!html_checks("<div>no namespace</div>"));
        assert!(!html_checks(
            "<div xmlns=\"http://example.org/other\">other namespace</div>"
        ));
        assert!(!html_checks(
            "<p xmlns=\"http://www.w3.org/1999/xhtml\">not a div</p>"
        ));
        assert!(!html_checks("plain text"));
        assert!(!html_checks(&format!("{}{}", div("a"), div("b"))));
    }

    #[test]
    fn a_narrative_needs_some_text() {
        assert!(!html_checks(&div("")));
        assert!(!html_checks(&div("  \n  ")));
        assert!(!html_checks(&div("<p></p>")));
    }

    #[test]
    fn script_and_other_elements_outside_the_basic_set_are_refused() {
        assert!(!html_checks(&div("<script>alert(1)</script>text")));
        assert!(!html_checks(&div(
            "<iframe src=\"https://x\"></iframe>text"
        )));
        assert!(!html_checks(&div("<form>text</form>")));
        assert!(!html_checks(&div("<p onclick=\"go()\">text</p>")));
        assert!(!html_checks(&div("<a href=\"javascript:go()\">text</a>")));
        assert!(!html_checks(&div("<a href=\" JavaScript:go()\">text</a>")));
    }

    #[test]
    fn a_narrative_that_is_not_well_formed_is_refused() {
        assert!(!html_checks(&div("<p>text")));
        assert!(!html_checks(&div("<p>text</b>")));
        assert!(!html_checks(&div("<p class=unquoted>text</p>")));
        assert!(!html_checks(
            "<div xmlns=\"http://www.w3.org/1999/xhtml\">text"
        ));
        assert!(!html_checks(&div("<!-- never closed text")));
    }
}
