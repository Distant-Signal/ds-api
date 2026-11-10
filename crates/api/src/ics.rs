//! A small iCalendar (RFC 5545) writer for the journey calendar feed and
//! the single-journey download (`routes::calendar`). Pure: takes a
//! [`Calendar`] and returns the document, so the tests can check its shape
//! without a database.
//!
//! Text values are escaped (`\`, `;`, `,` and line breaks), control
//! characters are dropped, and every content line is folded at 75 octets
//! (on a character boundary) with CRLF line ends, as the RFC requires.

use std::fmt::Write as _;

use chrono::{DateTime, NaiveDate, Utc};

/// When an event starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// A moment, written in UTC.
    At(DateTime<Utc>),
    /// A whole day (the calendar's own date, no time zone).
    Day(NaiveDate),
}

/// One alarm, shown before the event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alarm {
    /// Days before the event.
    pub days_before: u32,
    /// What the alarm says. A pop-up shows only this, not the event's own
    /// text, so it carries its own hedging.
    pub text: String,
}

/// One event.
#[derive(Debug, Clone)]
pub struct Event {
    /// A permanent id; the same event always has the same one.
    pub uid: String,
    pub start: When,
    /// The end of a timed event; `None` for a whole-day one or an event
    /// with a start only.
    pub end: Option<DateTime<Utc>>,
    pub summary: String,
    /// Plain text.
    pub description: String,
    pub alarm: Option<Alarm>,
}

/// One calendar document.
#[derive(Debug, Clone)]
pub struct Calendar {
    pub name: String,
    pub description: String,
    /// When the document was made (`DTSTAMP` on every event).
    pub generated: DateTime<Utc>,
    /// `PRODID`'s owner, so no other product's id is claimed.
    pub prod_id: String,
    pub events: Vec<Event>,
}

/// Escapes a TEXT value (RFC 5545 section 3.3.11) and drops characters a
/// content line may not carry.
pub fn escape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("\\n");
            }
            '\n' => out.push_str("\\n"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Folds one content line at 75 octets, never inside a character.
fn fold(line: &str, out: &mut String) {
    let mut width = 0;
    for c in line.chars() {
        let len = c.len_utf8();
        if width + len > 75 {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(c);
        width += len;
    }
    out.push_str("\r\n");
}

fn stamp(at: DateTime<Utc>) -> String {
    at.format("%Y%m%dT%H%M%SZ").to_string()
}

/// The `text/calendar` document for `calendar`.
pub fn render(calendar: &Calendar) -> String {
    let mut out = String::with_capacity(512 + calendar.events.len() * 400);
    let mut line = |text: String| fold(&text, &mut out);
    line("BEGIN:VCALENDAR".to_string());
    line("VERSION:2.0".to_string());
    line(format!("PRODID:{}", escape_text(&calendar.prod_id)));
    line("CALSCALE:GREGORIAN".to_string());
    line("METHOD:PUBLISH".to_string());
    line(format!("X-WR-CALNAME:{}", escape_text(&calendar.name)));
    line(format!(
        "X-WR-CALDESC:{}",
        escape_text(&calendar.description)
    ));
    // How often a subscribed calendar may refresh (RFC 7986); most clients
    // choose their own interval, so this is a hint.
    line("REFRESH-INTERVAL;VALUE=DURATION:PT12H".to_string());
    line("X-PUBLISHED-TTL:PT12H".to_string());
    for event in &calendar.events {
        line("BEGIN:VEVENT".to_string());
        line(format!("UID:{}", escape_text(&event.uid)));
        line(format!("DTSTAMP:{}", stamp(calendar.generated)));
        match event.start {
            When::At(at) => line(format!("DTSTART:{}", stamp(at))),
            When::Day(day) => line(format!("DTSTART;VALUE=DATE:{}", day.format("%Y%m%d"))),
        }
        if let Some(end) = event.end {
            line(format!("DTEND:{}", stamp(end)));
        }
        line(format!("SUMMARY:{}", escape_text(&event.summary)));
        line(format!("DESCRIPTION:{}", escape_text(&event.description)));
        line("TRANSP:TRANSPARENT".to_string());
        if let Some(alarm) = &event.alarm {
            line("BEGIN:VALARM".to_string());
            line("ACTION:DISPLAY".to_string());
            line(format!("DESCRIPTION:{}", escape_text(&alarm.text)));
            let mut trigger = String::new();
            let _ = write!(trigger, "TRIGGER:-P{}D", alarm.days_before);
            line(trigger);
            line("END:VALARM".to_string());
        }
        line("END:VEVENT".to_string());
    }
    line("END:VCALENDAR".to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn calendar(events: Vec<Event>) -> Calendar {
        Calendar {
            name: "Journeys".to_string(),
            description: "Plan".to_string(),
            generated: Utc.with_ymd_and_hms(2026, 11, 9, 12, 0, 0).unwrap(),
            prod_id: "-//Distant Signal//Journeys//EN".to_string(),
            events,
        }
    }

    #[test]
    fn text_is_escaped_and_controls_dropped() {
        assert_eq!(
            escape_text("a,b;c\\d\ne\r\nf\u{7}g"),
            "a\\,b\\;c\\\\d\\ne\\nfg"
        );
    }

    #[test]
    fn long_lines_fold_at_75_octets_on_a_character_boundary() {
        let mut out = String::new();
        fold(&format!("SUMMARY:{}", "é".repeat(100)), &mut out);
        for physical in out.trim_end_matches("\r\n").split("\r\n") {
            assert!(physical.len() <= 75, "{} octets", physical.len());
        }
        let unfolded = out.replace("\r\n ", "");
        assert_eq!(unfolded, format!("SUMMARY:{}\r\n", "é".repeat(100)));
    }

    #[test]
    fn a_document_has_crlf_line_ends_and_the_events() {
        let doc = render(&calendar(vec![
            Event {
                uid: "j1@example".to_string(),
                start: When::At(Utc.with_ymd_and_hms(2026, 11, 10, 8, 30, 0).unwrap()),
                end: None,
                summary: "Train: A to B".to_string(),
                description: "x".to_string(),
                alarm: None,
            },
            Event {
                uid: "r1@example".to_string(),
                start: When::Day(NaiveDate::from_ymd_opt(2026, 12, 1).unwrap()),
                end: None,
                summary: "Reminder".to_string(),
                description: "y".to_string(),
                alarm: Some(Alarm {
                    days_before: 7,
                    text: "Check the rules".to_string(),
                }),
            },
        ]));
        assert!(doc.starts_with("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n"));
        assert!(doc.ends_with("END:VCALENDAR\r\n"));
        assert!(!doc.replace("\r\n", "").contains('\n'));
        assert!(doc.contains("DTSTART:20261110T083000Z\r\n"));
        assert!(doc.contains("DTSTART;VALUE=DATE:20261201\r\n"));
        assert!(doc.contains("TRIGGER:-P7D\r\n"));
        assert!(doc.contains("ACTION:DISPLAY\r\nDESCRIPTION:Check the rules\r\n"));
        assert_eq!(doc.matches("BEGIN:VEVENT").count(), 2);
    }
}
