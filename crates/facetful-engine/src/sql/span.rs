//! Source spans and diagnostic rendering. Every token and AST node carries a
//! byte-offset span into the original query text; diagnostics render as a
//! caret-underlined snippet with an optional hint. This module is why the
//! hand-rolled parser can out-message the libraries: nothing here is generic.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
    pub fn to(self, other: Span) -> Span {
        Span::new(self.start, other.end.max(self.end))
    }
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub message: String,
    pub span: Span,
    pub hint: Option<String>,
}

impl Diagnostic {
    pub fn new(message: impl Into<String>, span: Span) -> Self {
        Self { message: message.into(), span, hint: None }
    }
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Render as: message, the offending source line, and a caret underline.
    /// Multi-line queries show the line containing the span start.
    pub fn render(&self, src: &str) -> String {
        let start = self.span.start.min(src.len());
        let end = self.span.end.clamp(start, src.len());
        let line_start = src[..start].rfind('\n').map_or(0, |i| i + 1);
        let line_end = src[start..].find('\n').map_or(src.len(), |i| start + i);
        let line_no = src[..start].bytes().filter(|&b| b == b'\n').count() + 1;
        let col = start - line_start + 1;
        let line = &src[line_start..line_end];
        let underline_len = (end - start).clamp(1, line_end.saturating_sub(start).max(1));

        let mut out = format!("error: {}\n", self.message);
        out.push_str(&format!("  --> line {line_no}, column {col}\n"));
        out.push_str(&format!("   | {line}\n"));
        out.push_str(&format!(
            "   | {}{}\n",
            " ".repeat(start - line_start),
            "^".repeat(underline_len)
        ));
        if let Some(h) = &self.hint {
            out.push_str(&format!("hint: {h}\n"));
        }
        out
    }
}

/// Edit distance for did-you-mean suggestions (binder). Small inputs only.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1].eq_ignore_ascii_case(&b[j - 1]) { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        core::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Best did-you-mean candidate from `options`, if any is close enough.
pub fn suggest<'a>(input: &str, options: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let max = (input.len() / 3).max(1).min(3);
    options
        .map(|o| (edit_distance(input, o), o))
        .filter(|(d, _)| *d <= max)
        .min_by_key(|(d, _)| *d)
        .map(|(_, o)| o)
}
