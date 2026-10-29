//! Markdown to sanitised HTML, for the web page. The output is meant to be dropped into the page with `innerHTML`,
//! so the rules are strict:
//!
//! - Raw HTML in the Markdown (block or inline) is never passed through; it is escaped and shows up as text.
//! - Links get `rel="noopener noreferrer"` and `target="_blank"`. A link whose scheme is not http, https or mailto
//!   (for example `javascript:` or `data:`) loses its `href`; its text stays.
//! - An image with such a scheme loses its `src`.
//!
//! Relative and fragment URLs are kept as they are.

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};

/// Render `markdown` to an HTML fragment (no `<html>` or `<body>`).
pub fn markdown_to_html(markdown: &str) -> String {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    // Marginalia links are never nested, so one flag tells the matching end tag what the start tag became.
    let mut link_is_anchor = true;
    let events = Parser::new_ext(markdown, options).map(move |event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        Event::Start(Tag::Link { dest_url, title, .. }) => {
            link_is_anchor = is_safe_url(&dest_url);
            if link_is_anchor {
                let mut tag = format!("<a href=\"{}\"", escape_html(&dest_url));
                if !title.is_empty() {
                    tag.push_str(&format!(" title=\"{}\"", escape_html(&title)));
                }
                tag.push_str(" rel=\"noopener noreferrer\" target=\"_blank\">");
                Event::Html(CowStr::from(tag))
            } else {
                Event::Html(CowStr::from("<span>"))
            }
        }
        Event::End(TagEnd::Link) => Event::Html(CowStr::from(if link_is_anchor { "</a>" } else { "</span>" })),
        Event::Start(Tag::Image { link_type, dest_url, title, id }) => {
            let dest_url = if is_safe_url(&dest_url) { dest_url } else { CowStr::from("") };
            Event::Start(Tag::Image { link_type, dest_url, title, id })
        }
        other => other,
    });
    let mut out = String::with_capacity(markdown.len() * 3 / 2);
    html::push_html(&mut out, events);
    out
}

/// Escape text for HTML element content and quoted attribute values.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Allow http, https and mailto, and URLs with no scheme at all (relative, `#fragment`, `//host`).
/// Browsers ignore ASCII whitespace and control characters inside a scheme (`java\tscript:`), so those are
/// stripped before looking.
fn is_safe_url(url: &str) -> bool {
    let compact: String = url.chars().filter(|c| !c.is_ascii_whitespace() && !c.is_control()).collect();
    match compact.find([':', '/', '?', '#']) {
        Some(i) if compact[i..].starts_with(':') => {
            let scheme = compact[..i].to_ascii_lowercase();
            matches!(scheme.as_str(), "http" | "https" | "mailto")
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_tags_are_escaped_block_and_inline() {
        let block = markdown_to_html("<script>alert(1)</script>\n\ntext");
        assert!(!block.contains("<script"), "{block}");
        assert!(block.contains("&lt;script&gt;alert(1)&lt;/script&gt;"), "{block}");

        let inline = markdown_to_html("hello <img src=x onerror=alert(1)> world");
        assert!(!inline.contains("<img"), "{inline}");
        assert!(inline.contains("&lt;img src=x onerror=alert(1)&gt;"), "{inline}");
    }

    #[test]
    fn javascript_links_lose_their_href() {
        for md in [
            "[x](javascript:alert(1))",
            "[x](JaVaScRiPt:alert(1))",
            "[x](  javascript:alert(1))",
            "[x](java&#9;script:alert(1))",
            "[x](&#106;avascript:alert(1))",
            "[x](data:text/html;base64,PHNjcmlwdD4=)",
            "[x](vbscript:msgbox)",
        ] {
            let out = markdown_to_html(md);
            assert!(!out.contains("href"), "{md} -> {out}");
            assert!(out.contains("<span>x</span>"), "{md} -> {out}");
        }
        let img = markdown_to_html("![alt](javascript:alert(1))");
        assert!(!img.contains("javascript"), "{img}");
    }

    #[test]
    fn safe_links_open_in_a_new_tab_without_opener() {
        let out = markdown_to_html("[a](https://example.com/?q=1&r=2 \"T\") [b](/rel) [c](#frag) [m](mailto:a@b.test)");
        assert!(out.contains("<a href=\"https://example.com/?q=1&amp;r=2\" title=\"T\" rel=\"noopener noreferrer\" target=\"_blank\">a</a>"), "{out}");
        assert_eq!(out.matches("rel=\"noopener noreferrer\" target=\"_blank\"").count(), 4, "{out}");
        assert!(out.contains("href=\"/rel\""), "{out}");
        assert!(out.contains("href=\"#frag\""), "{out}");
        assert!(out.contains("href=\"mailto:a@b.test\""), "{out}");
    }

    #[test]
    fn link_titles_cannot_break_out_of_the_attribute() {
        let out = markdown_to_html("[a](https://example.com \"x\\\" onmouseover=\\\"alert(1)\")");
        assert!(!out.contains("onmouseover=\"alert"), "{out}");
    }

    #[test]
    fn headings_and_lists_render() {
        let out = markdown_to_html("# Title\n\n## Sub\n\n- one\n- two\n\n1. first\n2. second\n\n**bold** and `code`");
        assert!(out.contains("<h1>Title</h1>"), "{out}");
        assert!(out.contains("<h2>Sub</h2>"), "{out}");
        assert!(out.contains("<ul>\n<li>one</li>\n<li>two</li>\n</ul>"), "{out}");
        assert!(out.contains("<ol>"), "{out}");
        assert!(out.contains("<strong>bold</strong>"), "{out}");
        assert!(out.contains("<code>code</code>"), "{out}");
    }

    #[test]
    fn escape_html_covers_attribute_characters() {
        assert_eq!(escape_html(r#"<a href="x">&'"#), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
    }
}
