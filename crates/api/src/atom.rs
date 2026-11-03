//! A small Atom 1.0 (RFC 4287) writer for the public feeds
//! (`routes::feeds`). Pure: takes a [`Feed`] and returns the document, so
//! the tests can check its shape without a database.
//!
//! Every value is XML-escaped, and characters XML 1.0 does not allow at all
//! (most C0 controls) are dropped, since entry text comes from third-party
//! feeds.

use std::fmt::Write as _;

use chrono::{DateTime, SecondsFormat, Utc};

/// One feed document.
#[derive(Debug, Clone)]
pub struct Feed {
    /// The feed's permanent id (a `tag:` URI).
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub updated: DateTime<Utc>,
    /// Where the feed itself is served (`rel="self"`).
    pub self_url: String,
    /// The page the feed follows (`rel="alternate"`).
    pub alternate_url: String,
    /// The credits and licence statements the feed's data needs.
    pub rights: String,
    pub author_name: String,
    pub author_uri: String,
    pub entries: Vec<Entry>,
}

/// One entry.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The entry's permanent id (a `tag:` URI).
    pub id: String,
    pub title: String,
    pub published: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    pub link: String,
    /// `(term, label)`.
    pub category: (String, String),
    /// Plain text.
    pub summary: String,
    /// HTML, built from escaped text only (`escape_text`).
    pub content_html: String,
    /// The credits for this entry's own data.
    pub rights: String,
}

/// The `application/atom+xml` document for `feed`.
pub fn render(feed: &Feed) -> String {
    let mut out = String::with_capacity(4096 + feed.entries.len() * 1024);
    out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str("<feed xmlns=\"http://www.w3.org/2005/Atom\" xml:lang=\"en-GB\">\n");
    element(&mut out, 1, "id", &feed.id);
    text_element(&mut out, 1, "title", &feed.title);
    text_element(&mut out, 1, "subtitle", &feed.subtitle);
    element(&mut out, 1, "updated", &timestamp(feed.updated));
    link(&mut out, 1, "self", "application/atom+xml", &feed.self_url);
    link(&mut out, 1, "alternate", "text/html", &feed.alternate_url);
    text_element(&mut out, 1, "rights", &feed.rights);
    out.push_str("  <author>\n");
    element(&mut out, 2, "name", &feed.author_name);
    element(&mut out, 2, "uri", &feed.author_uri);
    out.push_str("  </author>\n");
    for entry in &feed.entries {
        render_entry(&mut out, entry);
    }
    out.push_str("</feed>\n");
    out
}

fn render_entry(out: &mut String, entry: &Entry) {
    out.push_str("  <entry>\n");
    element(out, 2, "id", &entry.id);
    text_element(out, 2, "title", &entry.title);
    element(out, 2, "published", &timestamp(entry.published));
    element(out, 2, "updated", &timestamp(entry.updated));
    link(out, 2, "alternate", "text/html", &entry.link);
    let _ = writeln!(
        out,
        "    <category term=\"{}\" label=\"{}\"/>",
        escape_text(&entry.category.0),
        escape_text(&entry.category.1)
    );
    text_element(out, 2, "summary", &entry.summary);
    let _ = writeln!(
        out,
        "    <content type=\"html\">{}</content>",
        escape_text(&entry.content_html)
    );
    text_element(out, 2, "rights", &entry.rights);
    out.push_str("  </entry>\n");
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn element(out: &mut String, depth: usize, name: &str, value: &str) {
    indent(out, depth);
    let _ = writeln!(out, "<{name}>{}</{name}>", escape_text(value));
}

fn text_element(out: &mut String, depth: usize, name: &str, value: &str) {
    indent(out, depth);
    let _ = writeln!(out, "<{name} type=\"text\">{}</{name}>", escape_text(value));
}

fn link(out: &mut String, depth: usize, rel: &str, media_type: &str, href: &str) {
    indent(out, depth);
    let _ = writeln!(
        out,
        "<link rel=\"{rel}\" type=\"{media_type}\" href=\"{}\"/>",
        escape_text(href)
    );
}

/// RFC 3339, UTC, whole seconds (`2026-11-03T09:15:00Z`).
pub fn timestamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// `value` escaped for XML text and attribute values, with the characters
/// XML 1.0 forbids dropped.
pub fn escape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(ch),
            c if is_xml_char(c) => out.push(c),
            _ => {}
        }
    }
    out
}

/// XML 1.0's `Char` production, minus the whitespace handled above.
fn is_xml_char(c: char) -> bool {
    matches!(c, '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup_and_drops_forbidden_characters() {
        assert_eq!(
            escape_text("a<b>&\"c'\u{0}\u{1b}\u{fffe}\n"),
            "a&lt;b&gt;&amp;&quot;c&apos;\n"
        );
    }

    #[test]
    fn timestamps_are_utc_whole_seconds() {
        let at = DateTime::parse_from_rfc3339("2026-11-03T09:15:00.123+01:00")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(timestamp(at), "2026-11-03T08:15:00Z");
    }
}
