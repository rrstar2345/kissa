//! One message model, three renderings: Telegram HTML, email HTML and plain
//! text. Alerts and bot replies build a `Card`; each channel picks a rendering.
//!
//! Telegram refuses a message over 4096 *visible* characters (tags do not
//! count). The Telegram rendering is therefore built piece by piece against a
//! budget and ends with a "…" line when it runs out, so it never cuts inside a
//! tag or an entity. Free text (broker messages, error text) is also capped per
//! value before it is escaped.

/// Visible characters allowed in one Telegram message (the API limit is 4096).
pub const TELEGRAM_BUDGET: usize = 3800;
/// Longest single value or line kept; longer text is cut with "…".
const VALUE_MAX: usize = 400;
const TITLE_MAX: usize = 200;
const LABEL_MAX: usize = 40;
/// Visible size of the "more" marker plus its newline.
const MORE: &str = "… more";
const MORE_RESERVE: usize = 8;

/// Escape text for Telegram HTML and for email.
pub fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Cut to `max` characters on a character boundary, adding "…". Carriage
/// returns are dropped and surrounding whitespace trimmed.
pub fn cut(text: &str, max: usize) -> String {
    let clean: String = text.chars().filter(|c| *c != '\r').collect();
    let clean = clean.trim();
    if clean.chars().count() <= max {
        return clean.to_string();
    }
    let head: String = clean.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", head.trim_end())
}

/// `+₹1234.50` / `-₹300.00` / `₹0.00`.
pub fn rupees(v: f64) -> String {
    format!("{}₹{:.2}", if v < 0.0 { "-" } else if v > 0.0 { "+" } else { "" }, v.abs())
}

/// IST wall-clock time of day, `HH:MM:SS`.
pub fn ist_stamp() -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(5 * 3600 + 30 * 60)).format("%H:%M:%S").to_string()
}

#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    Text(String),
    /// A bold line with a blank line before it, to start a group.
    Heading(String),
}

/// What a message says, independent of how it is shown.
#[derive(Debug, Clone)]
pub struct Card {
    pub icon: &'static str,
    /// Hex colour of the rule above an email's title.
    pub accent: &'static str,
    /// Footer text, usually the event name. Empty = no footer.
    pub label: &'static str,
    pub title: String,
    /// Label / value rows shown first, as a small table in email.
    pub fields: Vec<(String, String)>,
    pub lines: Vec<Line>,
}

impl Card {
    pub fn new(icon: &'static str, accent: &'static str, label: &'static str, title: impl AsRef<str>) -> Self {
        Self { icon, accent, label, title: cut(title.as_ref(), TITLE_MAX), fields: Vec::new(), lines: Vec::new() }
    }

    /// Add a row. Blank values are skipped.
    pub fn add_field(&mut self, label: impl AsRef<str>, value: impl AsRef<str>) {
        let value = cut(value.as_ref(), VALUE_MAX);
        if !value.is_empty() {
            self.fields.push((cut(label.as_ref(), LABEL_MAX), value));
        }
    }

    /// Add text; every non-blank line of `text` becomes one line.
    pub fn add_text(&mut self, text: impl AsRef<str>) {
        for line in text.as_ref().lines() {
            let line = cut(line, VALUE_MAX);
            if !line.is_empty() {
                self.lines.push(Line::Text(line));
            }
        }
    }

    pub fn add_heading(&mut self, text: impl AsRef<str>) {
        let text = cut(text.as_ref(), VALUE_MAX);
        if !text.is_empty() {
            self.lines.push(Line::Heading(text));
        }
    }

    pub fn field(mut self, label: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        self.add_field(label, value);
        self
    }

    pub fn text(mut self, text: impl AsRef<str>) -> Self {
        self.add_text(text);
        self
    }

    fn footer(&self, stamp: &str) -> String {
        if self.label.is_empty() {
            format!("KISSA · {stamp} IST")
        } else {
            format!("KISSA · {} · {stamp} IST", self.label)
        }
    }

    // ------------------------------------------------------------- plain ---

    /// Fields and lines without the title: what the history list shows.
    pub fn plain_body(&self) -> String {
        let mut out = String::new();
        for (l, v) in &self.fields {
            out.push_str(&format!("{l}: {v}\n"));
        }
        for line in &self.lines {
            match line {
                Line::Text(t) => out.push_str(&format!("{t}\n")),
                Line::Heading(h) => out.push_str(&format!("\n{h}\n")),
            }
        }
        out.trim().to_string()
    }

    /// Title, fields and lines as plain text (email's text part, Telegram's fallback).
    pub fn plain(&self) -> String {
        let body = self.plain_body();
        if body.is_empty() {
            format!("KISSA: {}", self.title)
        } else {
            format!("KISSA: {}\n{body}", self.title)
        }
    }

    // ---------------------------------------------------------- telegram ---

    /// Telegram HTML (`<b>`, `<i>` only), within `TELEGRAM_BUDGET` visible characters.
    pub fn telegram_html(&self, stamp: &str) -> String {
        let mut pieces: Vec<(usize, String)> = Vec::new();
        pieces.push((
            self.icon.chars().count() + 1 + self.title.chars().count(),
            format!("{} <b>{}</b>", self.icon, esc(&self.title)),
        ));
        for (l, v) in &self.fields {
            pieces.push((l.chars().count() + 2 + v.chars().count(), format!("<b>{}</b>: {}", esc(l), esc(v))));
        }
        for line in &self.lines {
            match line {
                Line::Text(t) => pieces.push((t.chars().count(), esc(t))),
                // The blank line before a heading is one more visible character.
                Line::Heading(h) => pieces.push((h.chars().count() + 1, format!("\n<b>{}</b>", esc(h)))),
            }
        }
        let footer = self.footer(stamp);
        fit(pieces, (footer.chars().count(), format!("<i>{}</i>", esc(&footer))), TELEGRAM_BUDGET)
    }

    // ------------------------------------------------------------- email ---

    /// A self-contained HTML email: table layout and inline styles only, which
    /// is what mail clients render reliably.
    pub fn email_html(&self, stamp: &str) -> String {
        let mut rows = String::new();
        for (l, v) in &self.fields {
            rows.push_str(&format!(
                "<tr><td style=\"padding:3px 16px 3px 0;color:#6b7280;font-size:13px;white-space:nowrap;vertical-align:top;\">{}</td>\
                 <td style=\"padding:3px 0;color:#111827;font-size:14px;font-weight:600;\">{}</td></tr>",
                esc(l),
                esc(v)
            ));
        }
        let fields = if rows.is_empty() {
            String::new()
        } else {
            format!("<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" style=\"margin:0 0 10px 0;\">{rows}</table>")
        };
        let mut lines = String::new();
        for line in &self.lines {
            match line {
                Line::Text(t) => lines.push_str(&format!(
                    "<div style=\"color:#1f2937;font-size:14px;line-height:1.55;\">{}</div>",
                    esc(t)
                )),
                Line::Heading(h) => lines.push_str(&format!(
                    "<div style=\"color:#111827;font-size:14px;font-weight:700;margin-top:12px;\">{}</div>",
                    esc(h)
                )),
            }
        }
        format!(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"></head>\
             <body style=\"margin:0;padding:0;background:#f3f4f6;\">\
             <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"background:#f3f4f6;padding:16px 0;\"><tr><td align=\"center\">\
             <table role=\"presentation\" width=\"560\" cellpadding=\"0\" cellspacing=\"0\" style=\"width:100%;max-width:560px;background:#ffffff;border:1px solid #e5e7eb;border-radius:8px;font-family:-apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif;\">\
             <tr><td style=\"padding:16px 20px;border-top:4px solid {accent};border-radius:8px 8px 0 0;color:#111827;font-size:17px;font-weight:700;\">{icon} {title}</td></tr>\
             <tr><td style=\"padding:4px 20px 16px 20px;\">{fields}{lines}</td></tr>\
             <tr><td style=\"padding:10px 20px;border-top:1px solid #e5e7eb;color:#9ca3af;font-size:12px;\">{footer}</td></tr>\
             </table></td></tr></table></body></html>",
            accent = self.accent,
            icon = self.icon,
            title = esc(&self.title),
            fields = fields,
            lines = lines,
            footer = esc(&self.footer(stamp)),
        )
    }
}

/// Join pieces (visible length, markup) with newlines while they fit in
/// `budget`, then the footer after a blank line. When pieces do not fit, a
/// "… more" line is added instead of them.
fn fit(pieces: Vec<(usize, String)>, footer: (usize, String), budget: usize) -> String {
    let limit = budget.saturating_sub(footer.0 + 2);
    let mut out = String::new();
    let mut used = 0usize;
    for (i, (n, markup)) in pieces.into_iter().enumerate() {
        let need = n + usize::from(i > 0);
        // The title (piece 0) is capped well below the budget and always fits.
        if i > 0 && used + need + MORE_RESERVE > limit {
            out.push('\n');
            out.push_str(MORE);
            break;
        }
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&markup);
        used += need;
    }
    out.push_str("\n\n");
    out.push_str(&footer.1);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Characters Telegram would count: markup removed, entities decoded.
    fn visible(html: &str) -> usize {
        let mut text = String::new();
        let mut in_tag = false;
        for c in html.chars() {
            match c {
                '<' => in_tag = true,
                '>' if in_tag => in_tag = false,
                _ if !in_tag => text.push(c),
                _ => {}
            }
        }
        text.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").chars().count()
    }

    fn sample() -> Card {
        Card::new("✅", "#2e9e5b", "Fills", "Filled: BUY NIFTY 24000 CE 2026-11-24")
            .field("Account", "Paper 1")
            .field("Qty", "65 (1 lot)")
            .field("Price", "₹101.50")
            .text("Order filled 65 of 130")
    }

    #[test]
    fn escapes_markup_characters() {
        assert_eq!(esc("a<b>&\"c\""), "a&lt;b&gt;&amp;&quot;c&quot;");
        assert_eq!(esc("₹ 100"), "₹ 100");
    }

    #[test]
    fn cut_respects_characters_and_trims() {
        assert_eq!(cut("abc", 5), "abc");
        assert_eq!(cut("abcdef", 4), "abc…");
        assert_eq!(cut("₹₹₹₹₹", 3), "₹₹…");
        assert_eq!(cut("  a\r\nb  ", 10), "a\nb");
    }

    #[test]
    fn rupee_formatting() {
        assert_eq!(rupees(1234.5), "+₹1234.50");
        assert_eq!(rupees(-300.0), "-₹300.00");
        assert_eq!(rupees(0.0), "₹0.00");
    }

    #[test]
    fn blank_values_are_skipped() {
        let c = Card::new("✅", "#000000", "", "t").field("A", "  ").field("B", "x").text("\n\n");
        assert_eq!(c.fields, vec![("B".to_string(), "x".to_string())]);
        assert!(c.lines.is_empty());
    }

    #[test]
    fn telegram_layout() {
        let html = sample().telegram_html("10:04:12");
        let want = "✅ <b>Filled: BUY NIFTY 24000 CE 2026-11-24</b>\n\
                    <b>Account</b>: Paper 1\n\
                    <b>Qty</b>: 65 (1 lot)\n\
                    <b>Price</b>: ₹101.50\n\
                    Order filled 65 of 130\n\n\
                    <i>KISSA · Fills · 10:04:12 IST</i>";
        assert_eq!(html, want);
    }

    #[test]
    fn telegram_escapes_free_text() {
        let c = Card::new("❌", "#d64545", "Rejections", "Order rejected <NIFTY>").text("Broker said: qty < lot & price > 0");
        let html = c.telegram_html("09:15:00");
        assert!(html.contains("&lt;NIFTY&gt;"));
        assert!(html.contains("qty &lt; lot &amp; price &gt; 0"));
        assert!(!html.contains("<NIFTY>"));
    }

    #[test]
    fn telegram_never_exceeds_the_limit_or_breaks_tags() {
        let mut c = Card::new("📊", "#2f6fdb", "Daily P&L summary", "Daily P&L 2026-11-17: +₹1.00");
        for i in 0..300 {
            c.add_field(format!("Account {i}"), "x".repeat(500));
            c.add_text("y".repeat(500));
            c.add_heading(format!("Group {i} <&>"));
        }
        let html = c.telegram_html("15:45:00");
        assert!(visible(&html) <= 4096, "visible length {}", visible(&html));
        assert_eq!(html.matches("<b>").count(), html.matches("</b>").count());
        assert!(html.contains(MORE));
        assert!(html.ends_with("IST</i>"), "footer kept");
        // No entity is cut in half.
        assert_eq!(html.matches('&').count(), html.matches(';').count());
    }

    #[test]
    fn long_values_are_capped() {
        let c = Card::new("✅", "#000000", "", "t").field("Reason", "z".repeat(5000));
        assert_eq!(c.fields[0].1.chars().count(), VALUE_MAX);
        assert!(c.fields[0].1.ends_with('…'));
    }

    #[test]
    fn plain_text() {
        let p = sample().plain();
        assert_eq!(
            p,
            "KISSA: Filled: BUY NIFTY 24000 CE 2026-11-24\nAccount: Paper 1\nQty: 65 (1 lot)\nPrice: ₹101.50\nOrder filled 65 of 130"
        );
        assert_eq!(Card::new("🔔", "#000000", "", "Hello").plain(), "KISSA: Hello");
        assert_eq!(sample().plain_body(), "Account: Paper 1\nQty: 65 (1 lot)\nPrice: ₹101.50\nOrder filled 65 of 130");
    }

    #[test]
    fn headings_start_a_group() {
        let mut c = Card::new("📋", "#000000", "", "Positions");
        c.add_heading("Paper 1 (paper)");
        c.add_text("NIFTY 24000 CE · +65");
        assert_eq!(c.plain_body(), "Paper 1 (paper)\nNIFTY 24000 CE · +65");
        let html = c.telegram_html("10:00:00");
        assert!(html.contains("\n\n<b>Paper 1 (paper)</b>\nNIFTY 24000 CE · +65"));
    }

    #[test]
    fn email_html_is_escaped_and_complete() {
        let c = Card::new("❌", "#d64545", "Rejections", "Rejected <b>x</b>").field("Reason", "a & b").text("line <1>");
        let html = c.email_html("10:00:00");
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.ends_with("</html>"));
        assert!(html.contains("Rejected &lt;b&gt;x&lt;/b&gt;"));
        assert!(html.contains("a &amp; b"));
        assert!(html.contains("line &lt;1&gt;"));
        assert!(html.contains("#d64545"));
        assert!(html.contains("KISSA · Rejections · 10:00:00 IST"));
    }

    #[test]
    fn email_without_fields_has_no_table_of_rows() {
        let html = Card::new("🔔", "#6b7280", "", "Hi").text("body").email_html("10:00:00");
        assert!(!html.contains("white-space:nowrap"));
        assert!(html.contains("KISSA · 10:00:00 IST"));
    }
}
