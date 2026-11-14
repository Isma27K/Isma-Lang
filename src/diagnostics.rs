use crate::span::Span;
use ariadne::{Color, Config, IndexType, Label, Report, ReportKind, Source};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub span: Span,
    /// Message attached to the primary span (may be empty).
    pub primary_label: String,
    pub labels: Vec<(Span, String)>,
    pub notes: Vec<String>,
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            severity: Severity::Error,
            message: message.into(),
            span,
            primary_label: String::new(),
            labels: Vec::new(),
            notes: Vec::new(),
            help: None,
        }
    }

    pub fn warning(message: impl Into<String>, span: Span) -> Self {
        Diagnostic { severity: Severity::Warning, ..Diagnostic::error(message, span) }
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    pub fn with_primary(mut self, msg: impl Into<String>) -> Self {
        self.primary_label = msg.into();
        self
    }

    pub fn with_label(mut self, span: Span, msg: impl Into<String>) -> Self {
        self.labels.push((span, msg.into()));
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
}

/// Render diagnostics to a string, Rust-style.
pub fn render(diags: &[Diagnostic], path: &str, src: &str, color: bool) -> String {
    let mut out = Vec::new();
    let path = path.to_string();
    for d in diags {
        let config = Config::default().with_index_type(IndexType::Byte).with_color(color);
        let (kind, primary_color) = match d.severity {
            Severity::Error => (ReportKind::Error, Color::Red),
            Severity::Warning => (ReportKind::Warning, Color::Yellow),
        };
        let secondary_color = Color::Blue;
        let span = clamp(d.span, src);
        let mut primary = Label::new((path.clone(), span.range())).with_color(primary_color).with_order(0);
        if !d.primary_label.is_empty() {
            primary = primary.with_message(&d.primary_label);
        }
        let mut report = Report::build(kind, (path.clone(), span.range()))
            .with_config(config)
            .with_message(&d.message)
            .with_label(primary);
        for (i, (s, msg)) in d.labels.iter().enumerate() {
            let s = clamp(*s, src);
            report = report.with_label(
                Label::new((path.clone(), s.range()))
                    .with_message(msg)
                    .with_color(secondary_color)
                    .with_order(i as i32 + 1),
            );
        }
        for n in &d.notes {
            report = report.with_note(n);
        }
        if let Some(h) = &d.help {
            report = report.with_help(h);
        }
        let _ = report.finish().write((path.clone(), Source::from(src)), &mut out);
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn clamp(s: Span, src: &str) -> Span {
    let len = src.len() as u32;
    let start = s.start.min(len);
    let end = s.end.min(len).max(start);
    Span { start, end }
}
