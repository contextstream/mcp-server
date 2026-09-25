//! Terminal presentation for setup, doctor, and the other CLI screens.
//!
//! This is ContextCode's terminal design (contextcode `internal/tui/theme.go`
//! and `premium_chrome.go` at 7d56459) ported to line-oriented output: the
//! same color tokens, glyph vocabulary, kickers, cards, badges, key hints, and
//! selection rows. Rendering degrades by capability exactly as ContextCode
//! does: truecolor with a detected background paints card fills; truecolor
//! without one, 256-color, and 16-color terminals paint ink only (structure
//! comes from borders and glyphs, badges become reverse video); `NO_COLOR`,
//! `TERM=dumb`, and non-terminal output are plain text.

use std::fmt;
use std::io::IsTerminal;
use std::sync::OnceLock;
use std::time::Duration;

use console::measure_text_width;
use indicatif::{ProgressBar, ProgressStyle};

/// Left gutter shared by every line setup prints, so prompts, cards, and
/// activity lines align on one column.
pub const GUTTER: &str = "  ";

/// Cards never grow past ContextCode's onboarding card width.
const CARD_MAX_WIDTH: usize = 92;
const CARD_MIN_WIDTH: usize = 40;

const RESET: &str = "\x1b[0m";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorDepth {
    Plain,
    Ansi16,
    Ansi256,
    TrueColor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    Dark,
    Light,
}

/// ContextCode's semantic color tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tok {
    Surface,
    Raised,
    Selection,
    Fg,
    InkSoft,
    Muted,
    Faint,
    LineStrong,
    Accent,
    AccentInk,
    Chip,
    ChipInk,
    Success,
    Warning,
    Error,
    Secondary,
    BarTrack,
}

type Rgb = (u8, u8, u8);

/// (dark hex, light hex, xterm-256 dark, xterm-256 light, ANSI16 dark, ANSI16 light)
fn token(tok: Tok) -> (Rgb, Rgb, u8, u8, u8, u8) {
    match tok {
        Tok::Surface => ((0x11, 0x1a, 0x2d), (0xfe, 0xfe, 0xfd), 234, 255, 0, 15),
        Tok::Raised => ((0x16, 0x21, 0x3a), (0xf7, 0xf8, 0xfa), 235, 255, 0, 15),
        Tok::Selection => ((0x1c, 0x2d, 0x4d), (0xd2, 0xe5, 0xfe), 236, 189, 8, 7),
        Tok::Fg => ((0xdb, 0xe2, 0xed), (0x12, 0x2b, 0x43), 254, 235, 7, 0),
        Tok::InkSoft => ((0xb7, 0xc1, 0xd1), (0x2c, 0x45, 0x60), 251, 238, 7, 0),
        Tok::Muted => ((0x88, 0x94, 0xa9), (0x50, 0x67, 0x80), 103, 60, 8, 8),
        Tok::Faint => ((0x78, 0x84, 0x9c), (0x58, 0x6e, 0x86), 103, 66, 8, 8),
        Tok::LineStrong => ((0x2b, 0x3a, 0x57), (0xbf, 0xd2, 0xe7), 237, 252, 8, 7),
        Tok::Accent => ((0x6c, 0xa0, 0xf8), (0x09, 0x69, 0xc3), 75, 25, 12, 4),
        Tok::AccentInk => ((0xa0, 0xc7, 0xff), (0x0a, 0x5f, 0xae), 153, 25, 12, 4),
        Tok::Chip => ((0x42, 0x72, 0xd2), (0x08, 0x75, 0xdf), 62, 26, 4, 4),
        Tok::ChipInk => ((0xff, 0xff, 0xff), (0xff, 0xff, 0xff), 231, 231, 15, 15),
        Tok::Success => ((0x89, 0xd2, 0x9f), (0x17, 0x70, 0x4f), 115, 29, 2, 2),
        Tok::Warning => ((0xe6, 0xab, 0x6c), (0x8a, 0x5d, 0x0c), 179, 94, 3, 3),
        Tok::Error => ((0xf0, 0x90, 0x7e), (0xa8, 0x46, 0x3a), 210, 131, 1, 1),
        Tok::Secondary => ((0xb3, 0xa4, 0xe2), (0x6a, 0x4f, 0xb3), 146, 61, 13, 5),
        Tok::BarTrack => ((0x1d, 0x29, 0x44), (0xdd, 0xe4, 0xeb), 236, 254, 8, 7),
    }
}

/// Semantic line marks (ContextCode's glyph vocabulary).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// `✓` done
    Ok,
    /// `✗` failed
    Fail,
    /// `⚠` needs attention
    Warn,
    /// `●` informational / read
    Info,
    /// `▸` a step or action running
    Step,
    /// `○` not done yet
    Pending,
    /// `◆` a decision or guardrail
    Decision,
    /// `✦` a lesson or tip
    Tip,
}

impl Mark {
    fn glyph(self, unicode: bool) -> &'static str {
        match (self, unicode) {
            (Mark::Ok, true) => "✓",
            (Mark::Fail, true) => "✗",
            (Mark::Warn, true) => "⚠",
            (Mark::Info, true) => "●",
            (Mark::Step, true) => "▸",
            (Mark::Pending, true) => "○",
            (Mark::Decision, true) => "◆",
            (Mark::Tip, true) => "✦",
            (Mark::Ok, false) => "ok",
            (Mark::Fail, false) => "x",
            (Mark::Warn, false) => "!",
            (Mark::Info, false) => "*",
            (Mark::Step, false) => ">",
            (Mark::Pending, false) => "-",
            (Mark::Decision, false) => "*",
            (Mark::Tip, false) => "+",
        }
    }

    fn tok(self) -> Tok {
        match self {
            Mark::Ok | Mark::Tip => Tok::Success,
            Mark::Fail => Tok::Error,
            Mark::Warn | Mark::Decision => Tok::Warning,
            Mark::Info | Mark::Step => Tok::Accent,
            Mark::Pending => Tok::Faint,
        }
    }
}

/// Resolved terminal capabilities. Detection happens once per process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ui {
    depth: ColorDepth,
    variant: Variant,
    /// Background fills only when the real canvas is known (truecolor plus a
    /// detected background), as in ContextCode; elsewhere structure comes from
    /// borders and glyphs so nothing clashes with an unknown background.
    fill: bool,
    unicode: bool,
    width: usize,
}

static UI: OnceLock<Ui> = OnceLock::new();

/// The process-wide presentation settings.
pub fn ui() -> &'static Ui {
    UI.get_or_init(Ui::detect)
}

/// Pin the presentation settings for a test process (e.g. a render gallery).
/// Returns false when settings were already detected.
#[cfg(test)]
pub(crate) fn install_for_test(settings: Ui) -> bool {
    UI.set(settings).is_ok()
}

/// Environment inputs to capability detection, captured so detection is a
/// pure function in tests.
#[derive(Clone, Debug, Default)]
struct Probe {
    is_tty: bool,
    no_color: Option<String>,
    force_color: Option<String>,
    term: Option<String>,
    colorterm: Option<String>,
    windows_terminal: bool,
    /// Legacy Windows consoles (conhost with raster or Consolas fonts) lack
    /// glyphs like ✓ and ╭; modern hosts announce themselves.
    windows_legacy_console: bool,
    theme_override: Option<String>,
    ascii: bool,
}

impl Probe {
    fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok();
        Self {
            is_tty: std::io::stdout().is_terminal(),
            no_color: var("NO_COLOR"),
            force_color: var("CLICOLOR_FORCE").or_else(|| var("FORCE_COLOR")),
            term: var("TERM"),
            colorterm: var("COLORTERM"),
            windows_terminal: var("WT_SESSION").is_some(),
            windows_legacy_console: cfg!(windows)
                && var("WT_SESSION").is_none()
                && var("TERM_PROGRAM").is_none()
                && var("ConEmuANSI").is_none()
                && var("TERM").is_none(),
            theme_override: var("CONTEXTSTREAM_THEME"),
            ascii: var("CONTEXTSTREAM_ASCII").is_some_and(|value| truthy(&value)),
        }
    }

    fn depth(&self) -> ColorDepth {
        if self
            .theme_override
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("none"))
        {
            return ColorDepth::Plain;
        }
        let forced = self.force_color.as_deref().is_some_and(truthy);
        // Like ContextCode, NO_COLOR counts only when set and non-empty.
        if !forced && self.no_color.as_deref().is_some_and(|v| !v.is_empty()) {
            return ColorDepth::Plain;
        }
        let term = self.term.as_deref().unwrap_or("");
        if !forced && (!self.is_tty || term == "dumb") {
            return ColorDepth::Plain;
        }
        let colorterm = self.colorterm.as_deref().unwrap_or("").to_ascii_lowercase();
        if colorterm.contains("truecolor") || colorterm.contains("24bit") || self.windows_terminal {
            ColorDepth::TrueColor
        } else if term.contains("256color") {
            ColorDepth::Ansi256
        } else {
            ColorDepth::Ansi16
        }
    }

    fn unicode(&self) -> bool {
        !self.ascii && !self.windows_legacy_console && self.term.as_deref() != Some("linux")
    }
}

fn truthy(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

impl Ui {
    fn detect() -> Self {
        let probe = Probe::from_env();
        let mut depth = probe.depth();
        // Windows consoles only interpret escapes once virtual-terminal mode
        // is on; console enables it as a side effect of this check.
        if cfg!(windows) && depth != ColorDepth::Plain && !console::colors_enabled() {
            depth = ColorDepth::Plain;
        }
        let width = console::Term::stdout()
            .size_checked()
            .map(|(_, cols)| cols as usize)
            .unwrap_or(80);

        let explicit = match probe.theme_override.as_deref().map(str::to_ascii_lowercase) {
            Some(value) if value == "light" => Some(Variant::Light),
            Some(value) if value == "dark" => Some(Variant::Dark),
            _ => None,
        };
        let (variant, canvas_known) = match explicit {
            Some(variant) => (variant, false),
            None if depth != ColorDepth::Plain && std::io::stdin().is_terminal() => {
                query_terminal_variant()
            }
            None => (Variant::Dark, false),
        };

        Self {
            depth,
            variant,
            fill: depth == ColorDepth::TrueColor && canvas_known,
            unicode: probe.unicode(),
            width,
        }
    }

    /// Fixed settings, for tests and for callers that must not probe.
    pub fn fixed(depth: ColorDepth, variant: Variant, fill: bool, width: usize) -> Self {
        Self {
            depth,
            variant,
            fill: fill && depth == ColorDepth::TrueColor,
            unicode: true,
            width,
        }
    }

    pub fn plain(width: usize) -> Self {
        Self::fixed(ColorDepth::Plain, Variant::Dark, false, width)
    }

    pub fn is_plain(&self) -> bool {
        self.depth == ColorDepth::Plain
    }

    pub fn unicode(&self) -> bool {
        self.unicode
    }

    fn fg_code(&self, tok: Tok) -> Option<String> {
        // On an unknown canvas, body ink stays the terminal's own foreground,
        // which is always legible on the terminal's own background.
        if !self.fill && matches!(tok, Tok::Fg | Tok::InkSoft) {
            return None;
        }
        let (dark, light, c256_dark, c256_light, c16_dark, c16_light) = token(tok);
        let light_variant = self.variant == Variant::Light;
        match self.depth {
            ColorDepth::Plain => None,
            ColorDepth::TrueColor => {
                let (r, g, b) = if light_variant { light } else { dark };
                Some(format!("38;2;{r};{g};{b}"))
            }
            ColorDepth::Ansi256 => Some(format!(
                "38;5;{}",
                if light_variant { c256_light } else { c256_dark }
            )),
            ColorDepth::Ansi16 => {
                let n = if light_variant { c16_light } else { c16_dark };
                Some(if n < 8 {
                    format!("{}", 30 + n)
                } else {
                    format!("{}", 90 + n - 8)
                })
            }
        }
    }

    fn bg_code(&self, tok: Tok) -> Option<String> {
        if !self.fill {
            return None;
        }
        let (dark, light, ..) = token(tok);
        let (r, g, b) = if self.variant == Variant::Light {
            light
        } else {
            dark
        };
        Some(format!("48;2;{r};{g};{b}"))
    }

    /// Paint `text` with a foreground token and optional bold.
    pub fn paint(&self, text: &str, fg: Option<Tok>, bold: bool) -> String {
        self.paint_full(text, fg, None, bold, false)
    }

    fn paint_full(
        &self,
        text: &str,
        fg: Option<Tok>,
        bg: Option<Tok>,
        bold: bool,
        reverse: bool,
    ) -> String {
        if self.is_plain() || text.is_empty() {
            return text.to_string();
        }
        let mut codes: Vec<String> = Vec::new();
        if bold {
            codes.push("1".into());
        }
        if reverse {
            codes.push("7".into());
        }
        if let Some(code) = fg.and_then(|tok| self.fg_code(tok)) {
            codes.push(code);
        }
        if let Some(code) = bg.and_then(|tok| self.bg_code(tok)) {
            codes.push(code);
        }
        if codes.is_empty() {
            return text.to_string();
        }
        format!("\x1b[{}m{text}{RESET}", codes.join(";"))
    }

    pub fn accent(&self, text: &str) -> String {
        self.paint(text, Some(Tok::Accent), false)
    }

    /// File paths, URLs, and commands.
    pub fn path(&self, text: &str) -> String {
        self.paint(text, Some(Tok::AccentInk), false)
    }

    pub fn muted(&self, text: &str) -> String {
        self.paint(text, Some(Tok::Muted), false)
    }

    pub fn faint(&self, text: &str) -> String {
        self.paint(text, Some(Tok::Faint), false)
    }

    pub fn strong(&self, text: &str) -> String {
        self.paint(text, Some(Tok::Fg), true)
    }

    pub fn success(&self, text: &str) -> String {
        self.paint(text, Some(Tok::Success), false)
    }

    pub fn warning(&self, text: &str) -> String {
        self.paint(text, Some(Tok::Warning), false)
    }

    pub fn error(&self, text: &str) -> String {
        self.paint(text, Some(Tok::Error), false)
    }

    pub fn mark(&self, mark: Mark) -> String {
        self.paint(mark.glyph(self.unicode), Some(mark.tok()), true)
    }

    /// Bold accent screen title ("Sign in to ContextStream").
    pub fn heading(&self, text: &str) -> String {
        self.paint(text, Some(Tok::Accent), true)
    }

    /// Plain uppercase section label in Faint (ContextCode's kicker; no
    /// letter-spacing, monospace already carries the idiom).
    pub fn kicker(&self, text: &str) -> String {
        self.paint(&text.to_uppercase(), Some(Tok::Faint), false)
    }

    /// Activity line: mark, head in ink, the detail after ` · ` in Faint.
    pub fn line(&self, mark: Mark, head: &str, tail: Option<&str>) -> String {
        let mut out = format!("{GUTTER}{} {}", self.mark(mark), head);
        if let Some(tail) = tail.filter(|tail| !tail.is_empty()) {
            out.push_str(&self.faint(&format!(" · {tail}")));
        }
        out
    }

    /// A solid chip badge (` PLAN `); reverse video without a canvas.
    pub fn badge(&self, text: &str) -> String {
        let label = format!(" {} ", text.to_uppercase());
        if self.is_plain() {
            return format!("[{}]", text.to_uppercase());
        }
        if self.fill {
            self.paint_full(&label, Some(Tok::ChipInk), Some(Tok::Chip), true, false)
        } else {
            self.paint_full(&label, Some(Tok::Accent), None, true, true)
        }
    }

    /// `key label` pairs joined by ` · `: key in ink, label in Muted.
    pub fn keys(&self, pairs: &[(&str, &str)]) -> String {
        pairs
            .iter()
            .map(|(key, label)| format!("{} {}", self.strong(key), self.muted(label)))
            .collect::<Vec<_>>()
            .join(&self.faint(" · "))
    }

    /// `1 ACCOUNT · 2 Editors · 3 Project`: the current step in capitals.
    pub fn stepper(&self, steps: &[&str], current: usize) -> String {
        steps
            .iter()
            .enumerate()
            .map(|(index, step)| {
                let number = index + 1;
                if number == current {
                    self.paint(
                        &format!("{number} {}", step.to_uppercase()),
                        Some(Tok::Accent),
                        true,
                    )
                } else if number < current {
                    format!(
                        "{} {}",
                        self.paint(Mark::Ok.glyph(self.unicode), Some(Tok::Success), false),
                        self.muted(step)
                    )
                } else {
                    self.faint(&format!("{number} {step}"))
                }
            })
            .collect::<Vec<_>>()
            .join(&self.faint(" · "))
    }

    /// A label/value row inside a summary card.
    pub fn row(&self, label: &str, value: &str) -> String {
        let pad = 12usize.saturating_sub(measure_text_width(label));
        format!("{}{}{value}", self.faint(label), " ".repeat(pad))
    }

    /// Horizontal meter in `▆` cells; the track uses the dim bar color.
    pub fn bar(&self, fraction: f64, cells: usize) -> String {
        let filled = ((fraction.clamp(0.0, 1.0) * cells as f64).round() as usize).min(cells);
        let cell = if self.unicode { "▆" } else { "#" };
        let track = if self.unicode { "▆" } else { "." };
        format!(
            "{}{}",
            self.paint(&cell.repeat(filled), Some(Tok::Accent), false),
            self.paint(&track.repeat(cells - filled), Some(Tok::BarTrack), false)
        )
    }

    /// Hairline rule across the card width.
    pub fn rule(&self) -> String {
        let width = self.card_width().saturating_sub(GUTTER.len());
        let line = if self.unicode { "─" } else { "-" };
        format!(
            "{GUTTER}{}",
            self.paint(&line.repeat(width), Some(Tok::LineStrong), false)
        )
    }

    pub fn card_width(&self) -> usize {
        self.width
            .saturating_sub(1)
            .clamp(CARD_MIN_WIDTH, CARD_MAX_WIDTH)
    }

    /// A rounded card around `body`, padded one row and two columns. With a
    /// known canvas the Surface fill covers the border cells too, and every
    /// inner reset re-applies the fill so styled spans never punch holes.
    pub fn card(&self, body: &[String]) -> String {
        let outer = self.card_width().saturating_sub(GUTTER.len());
        let inner = outer.saturating_sub(6);
        let (tl, tr, bl, br, h, v) = if self.unicode {
            ("╭", "╮", "╰", "╯", "─", "│")
        } else {
            ("+", "+", "+", "+", "-", "|")
        };
        let border = |text: &str| {
            self.paint_full(
                text,
                Some(Tok::LineStrong),
                Some(Tok::Surface),
                false,
                false,
            )
        };
        let fill_prefix = self.fill_prefix();

        let mut lines = Vec::with_capacity(body.len() + 4);
        lines.push(format!(
            "{GUTTER}{}",
            border(&format!("{tl}{}{tr}", h.repeat(outer - 2)))
        ));
        let mut push_body = |content: &str| {
            let content = fit(content, inner);
            let pad = inner.saturating_sub(measure_text_width(&content));
            let styled = match &fill_prefix {
                Some(prefix) => format!(
                    "{prefix}  {}{}  {RESET}",
                    content.replace(RESET, &format!("{RESET}{prefix}")),
                    " ".repeat(pad)
                ),
                None => format!("  {content}{}  ", " ".repeat(pad)),
            };
            lines.push(format!("{GUTTER}{}{styled}{}", border(v), border(v)));
        };
        push_body("");
        for line in body {
            push_body(line);
        }
        push_body("");
        lines.push(format!(
            "{GUTTER}{}",
            border(&format!("{bl}{}{br}", h.repeat(outer - 2)))
        ));
        lines.join("\n")
    }

    fn fill_prefix(&self) -> Option<String> {
        let bg = self.bg_code(Tok::Surface)?;
        let fg = self
            .fg_code(Tok::Fg)
            .map(|fg| format!(";{fg}"))
            .unwrap_or_default();
        Some(format!("\x1b[{bg}{fg}m"))
    }

    /// One-line status bar: badge, ` │ `-joined segments, right-aligned tail.
    pub fn status_bar(&self, badge: &str, segments: &[String], right: &str) -> String {
        let sep = self.paint(
            if self.unicode { " │ " } else { " | " },
            Some(Tok::LineStrong),
            false,
        );
        let left = std::iter::once(self.badge(badge))
            .chain(segments.iter().cloned())
            .collect::<Vec<_>>()
            .join(&sep);
        let right = self.muted(right);
        let used = measure_text_width(&left) + measure_text_width(&right) + GUTTER.len();
        let gap = self.card_width().saturating_sub(used).max(2);
        format!("{GUTTER}{left}{}{right}", " ".repeat(gap))
    }

    /// ContextCode's dot spinner (`.  ` `.. ` `...` at 500ms) in Accent.
    pub fn spinner(&self, message: &str) -> ProgressBar {
        let bar = ProgressBar::new_spinner();
        let frames = [".  ", ".. ", "...", "   "];
        let ticks: Vec<String> = frames.iter().map(|frame| self.accent(frame)).collect();
        let tick_refs: Vec<&str> = ticks.iter().map(String::as_str).collect();
        bar.set_style(
            ProgressStyle::with_template(&format!("{GUTTER}{{msg}}{{spinner}}"))
                .unwrap_or_else(|_| ProgressStyle::default_spinner())
                .tick_strings(&tick_refs),
        );
        bar.set_message(message.to_string());
        bar.enable_steady_tick(Duration::from_millis(500));
        bar
    }
}

/// Truncate a styled line to `width` visible columns with an ellipsis.
fn fit(content: &str, width: usize) -> String {
    if measure_text_width(content) <= width {
        return content.to_string();
    }
    console::truncate_str(content, width, "…").into_owned()
}

/// Soft-wrap plain text to `width` columns on word boundaries.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            let needed = measure_text_width(&current)
                + usize::from(!current.is_empty())
                + measure_text_width(word);
            if !current.is_empty() && needed > width {
                lines.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        lines.push(current);
    }
    lines
}

/// Ask the terminal for its background (OSC 11) to choose the dark or light
/// variant; a known background also enables card fills. Terminals that do
/// not answer fall back to the dark variant without fills.
fn query_terminal_variant() -> (Variant, bool) {
    let mut options = terminal_colorsaurus::QueryOptions::default();
    options.timeout = Duration::from_millis(250);
    match terminal_colorsaurus::color_palette(options) {
        Ok(palette) => match palette.theme_mode() {
            terminal_colorsaurus::ThemeMode::Light => (Variant::Light, true),
            terminal_colorsaurus::ThemeMode::Dark => (Variant::Dark, true),
        },
        Err(_) => (Variant::Dark, false),
    }
}

/// Run `future` behind the ContextCode spinner, clearing it when done.
pub async fn spin<F, T>(message: &str, future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let bar = ui().spinner(message);
    let result = future.await;
    bar.finish_and_clear();
    result
}

/// Print an activity line.
pub fn say(mark: Mark, head: &str, tail: Option<&str>) {
    println!("{}", ui().line(mark, head, tail));
}

/// dialoguer theme matching ContextCode's pickers: `❯` prompts with inline
/// key hints, ` › ` on the Selection fill for the active row, `●`/`○` for
/// multi-select, and a collapsed `✓ prompt · answer` line once answered.
pub struct PromptTheme {
    ui: Ui,
}

impl PromptTheme {
    pub fn new(ui: Ui) -> Self {
        Self { ui }
    }

    fn prompt_head(&self, f: &mut dyn fmt::Write, prompt: &str) -> fmt::Result {
        let caret = if self.ui.unicode { "❯" } else { ">" };
        write!(
            f,
            "{GUTTER}{} {}",
            self.ui.paint(caret, Some(Tok::Accent), true),
            self.ui.strong(prompt.trim_end_matches(':'))
        )
    }

    fn answered(&self, f: &mut dyn fmt::Write, prompt: &str, answer: &str) -> fmt::Result {
        write!(
            f,
            "{GUTTER}{} {}",
            self.ui.mark(Mark::Ok),
            self.ui.muted(prompt.trim_end_matches(':'))
        )?;
        if !answer.is_empty() {
            write!(f, "{}{}", self.ui.faint(" · "), self.ui.accent(answer))?;
        }
        Ok(())
    }

    fn hint(&self, f: &mut dyn fmt::Write, pairs: &[(&str, &str)]) -> fmt::Result {
        write!(f, "   {}", self.ui.keys(pairs))
    }
}

impl dialoguer::theme::Theme for PromptTheme {
    fn format_prompt(&self, f: &mut dyn fmt::Write, prompt: &str) -> fmt::Result {
        self.prompt_head(f, prompt)
    }

    fn format_error(&self, f: &mut dyn fmt::Write, err: &str) -> fmt::Result {
        write!(
            f,
            "{GUTTER}{} {}",
            self.ui.mark(Mark::Fail),
            self.ui.error(err)
        )
    }

    fn format_confirm_prompt(
        &self,
        f: &mut dyn fmt::Write,
        prompt: &str,
        default: Option<bool>,
    ) -> fmt::Result {
        self.prompt_head(f, prompt)?;
        let choices = match default {
            Some(true) => "Y/n",
            Some(false) => "y/N",
            None => "y/n",
        };
        write!(f, " {} ", self.ui.faint(choices))
    }

    fn format_confirm_prompt_selection(
        &self,
        f: &mut dyn fmt::Write,
        prompt: &str,
        selection: Option<bool>,
    ) -> fmt::Result {
        let answer = match selection {
            Some(true) => "yes",
            Some(false) => "no",
            None => "",
        };
        self.answered(f, prompt, answer)
    }

    fn format_input_prompt(
        &self,
        f: &mut dyn fmt::Write,
        prompt: &str,
        default: Option<&str>,
    ) -> fmt::Result {
        self.prompt_head(f, prompt)?;
        if let Some(default) = default.filter(|default| !default.is_empty()) {
            write!(f, " {}", self.ui.faint(&format!("({default})")))?;
        }
        let pointer = if self.ui.unicode { "›" } else { ">" };
        write!(f, " {} ", self.ui.accent(pointer))
    }

    fn format_input_prompt_selection(
        &self,
        f: &mut dyn fmt::Write,
        prompt: &str,
        sel: &str,
    ) -> fmt::Result {
        self.answered(f, prompt, sel)
    }

    fn format_password_prompt(&self, f: &mut dyn fmt::Write, prompt: &str) -> fmt::Result {
        self.format_input_prompt(f, prompt, None)
    }

    fn format_password_prompt_selection(
        &self,
        f: &mut dyn fmt::Write,
        prompt: &str,
    ) -> fmt::Result {
        self.answered(
            f,
            prompt,
            if self.ui.unicode {
                "••••••••"
            } else {
                "********"
            },
        )
    }

    fn format_select_prompt(&self, f: &mut dyn fmt::Write, prompt: &str) -> fmt::Result {
        self.prompt_head(f, prompt)?;
        self.hint(f, &[("↑/↓", "move"), ("Enter", "choose")])
    }

    fn format_select_prompt_selection(
        &self,
        f: &mut dyn fmt::Write,
        prompt: &str,
        sel: &str,
    ) -> fmt::Result {
        self.answered(f, prompt, sel)
    }

    fn format_multi_select_prompt(&self, f: &mut dyn fmt::Write, prompt: &str) -> fmt::Result {
        self.prompt_head(f, prompt)?;
        self.hint(f, &[("Space", "toggle"), ("Enter", "confirm")])
    }

    fn format_multi_select_prompt_selection(
        &self,
        f: &mut dyn fmt::Write,
        prompt: &str,
        selections: &[&str],
    ) -> fmt::Result {
        let answer = if selections.is_empty() {
            "none".to_string()
        } else {
            selections.join(", ")
        };
        self.answered(f, prompt, &answer)
    }

    fn format_select_prompt_item(
        &self,
        f: &mut dyn fmt::Write,
        text: &str,
        active: bool,
    ) -> fmt::Result {
        let pointer = if self.ui.unicode { "›" } else { ">" };
        if active {
            let row = format!(" {pointer} {text} ");
            write!(
                f,
                "{GUTTER}{}",
                self.ui
                    .paint_full(&row, Some(Tok::Fg), Some(Tok::Selection), true, false)
            )
        } else {
            write!(f, "{GUTTER}   {text}")
        }
    }

    fn format_multi_select_prompt_item(
        &self,
        f: &mut dyn fmt::Write,
        text: &str,
        checked: bool,
        active: bool,
    ) -> fmt::Result {
        let pointer = if self.ui.unicode { "›" } else { ">" };
        let check = if checked {
            self.ui.paint(
                if self.ui.unicode { "●" } else { "[x]" },
                Some(Tok::Accent),
                true,
            )
        } else {
            self.ui.faint(if self.ui.unicode { "○" } else { "[ ]" })
        };
        if active {
            write!(
                f,
                "{GUTTER}{} {check} {}",
                self.ui.paint(pointer, Some(Tok::Accent), true),
                self.ui.strong(text)
            )
        } else {
            write!(f, "{GUTTER}  {check} {text}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dialoguer::theme::Theme;

    fn strip(text: &str) -> String {
        console::strip_ansi_codes(text).into_owned()
    }

    fn probe(tty: bool) -> Probe {
        Probe {
            is_tty: tty,
            term: Some("xterm-256color".into()),
            ..Probe::default()
        }
    }

    #[test]
    fn detection_follows_contextcode_capability_rules() {
        assert_eq!(probe(true).depth(), ColorDepth::Ansi256);
        assert_eq!(probe(false).depth(), ColorDepth::Plain);

        let truecolor = Probe {
            colorterm: Some("truecolor".into()),
            ..probe(true)
        };
        assert_eq!(truecolor.depth(), ColorDepth::TrueColor);

        // NO_COLOR only counts when non-empty.
        let empty_no_color = Probe {
            no_color: Some(String::new()),
            ..probe(true)
        };
        assert_eq!(empty_no_color.depth(), ColorDepth::Ansi256);
        let no_color = Probe {
            no_color: Some("1".into()),
            ..probe(true)
        };
        assert_eq!(no_color.depth(), ColorDepth::Plain);

        let dumb = Probe {
            term: Some("dumb".into()),
            ..probe(true)
        };
        assert_eq!(dumb.depth(), ColorDepth::Plain);

        let forced = Probe {
            force_color: Some("1".into()),
            ..probe(false)
        };
        assert_eq!(forced.depth(), ColorDepth::Ansi256);

        let opted_out = Probe {
            theme_override: Some("none".into()),
            ..probe(true)
        };
        assert_eq!(opted_out.depth(), ColorDepth::Plain);
    }

    #[test]
    fn glyphs_fall_back_to_ascii_where_fonts_lack_them() {
        assert!(probe(true).unicode());
        let legacy_windows = Probe {
            windows_legacy_console: true,
            ..probe(true)
        };
        assert!(!legacy_windows.unicode());
        let linux_console = Probe {
            term: Some("linux".into()),
            ..probe(true)
        };
        assert!(!linux_console.unicode());
        let opted_out = Probe {
            ascii: true,
            ..probe(true)
        };
        assert!(!opted_out.unicode());
    }

    #[test]
    fn plain_output_has_no_escapes_but_keeps_structure() {
        let ui = Ui::plain(80);
        let line = ui.line(Mark::Ok, "Signed in", Some("erik@example.com"));
        assert_eq!(line, "  ✓ Signed in · erik@example.com");
        assert_eq!(ui.badge("plan"), "[PLAN]");
        assert!(!ui.card(&["hello".into()]).contains('\x1b'));
    }

    #[test]
    fn truecolor_uses_the_exact_contextcode_tokens() {
        let dark = Ui::fixed(ColorDepth::TrueColor, Variant::Dark, false, 80);
        assert!(dark.accent("x").contains("38;2;108;160;248"));
        assert!(dark.success("x").contains("38;2;137;210;159"));
        let light = Ui::fixed(ColorDepth::TrueColor, Variant::Light, false, 80);
        assert!(light.accent("x").contains("38;2;9;105;195"));
        // Body ink stays the terminal default without a known canvas.
        assert_eq!(dark.paint("body", Some(Tok::Fg), false), "body");
        let filled = Ui::fixed(ColorDepth::TrueColor, Variant::Dark, true, 80);
        assert!(filled
            .paint("body", Some(Tok::Fg), false)
            .contains("38;2;219;226;237"));
    }

    #[test]
    fn reduced_depths_use_indexed_colors_and_reverse_video_badges() {
        let c256 = Ui::fixed(ColorDepth::Ansi256, Variant::Dark, true, 80);
        assert!(c256.accent("x").contains("38;5;75"));
        assert!(c256.badge("plan").contains("\x1b[1;7;"));
        let c16 = Ui::fixed(ColorDepth::Ansi16, Variant::Dark, false, 80);
        assert!(c16.accent("x").contains("\x1b[94m"));
        assert!(c16.error("x").contains("\x1b[31m"));
    }

    #[test]
    fn filled_badge_uses_chip_colors() {
        let ui = Ui::fixed(ColorDepth::TrueColor, Variant::Dark, true, 80);
        let badge = ui.badge("plan");
        assert!(badge.contains("48;2;66;114;210"), "{badge:?}");
        assert_eq!(strip(&badge), " PLAN ");
    }

    #[test]
    fn cards_have_a_fixed_visible_width_in_every_mode() {
        for ui in [
            Ui::plain(120),
            Ui::fixed(ColorDepth::Ansi256, Variant::Dark, false, 120),
            Ui::fixed(ColorDepth::TrueColor, Variant::Dark, true, 120),
        ] {
            let long = "x".repeat(200);
            let card = ui.card(&[ui.heading("Title"), ui.muted("detail"), long]);
            let widths: Vec<usize> = card.lines().map(measure_text_width).collect();
            assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
            assert_eq!(widths[0], CARD_MAX_WIDTH);
            assert!(strip(&card).contains('…'));
        }
    }

    #[test]
    fn filled_cards_reapply_the_fill_after_inner_resets() {
        let ui = Ui::fixed(ColorDepth::TrueColor, Variant::Dark, true, 80);
        let card = ui.card(&[format!("{} tail", ui.accent("styled"))]);
        let body = card.lines().nth(2).expect("body line");
        let fill = "48;2;17;26;45";
        let after_first_reset = body.split(RESET).nth(2).expect("segment after styled span");
        assert!(after_first_reset.contains(fill), "{body:?}");
    }

    #[test]
    fn narrow_terminals_clamp_card_width() {
        let ui = Ui::plain(30);
        let card = ui.card(&["hi".into()]);
        assert_eq!(
            measure_text_width(card.lines().next().unwrap()),
            CARD_MIN_WIDTH
        );
    }

    #[test]
    fn stepper_capitalizes_only_the_current_step() {
        let ui = Ui::plain(80);
        assert_eq!(
            ui.stepper(&["Account", "Editors", "Project", "Review"], 2),
            "✓ Account · 2 EDITORS · 3 Project · 4 Review"
        );
    }

    #[test]
    fn bar_fills_proportionally() {
        let ui = Ui::plain(80);
        assert_eq!(ui.bar(0.5, 10), "▆▆▆▆▆▆▆▆▆▆");
        let ui = Ui::fixed(ColorDepth::Ansi256, Variant::Dark, false, 80);
        let bar = ui.bar(0.25, 8);
        assert_eq!(strip(&bar).chars().count(), 8);
        assert!(bar.contains("38;5;75"));
    }

    #[test]
    fn wrap_breaks_on_words() {
        assert_eq!(
            wrap("one two three four five", 10),
            vec!["one two".to_string(), "three four".into(), "five".into()]
        );
        // Narrower requests are clamped so a word never wraps per letter.
        assert_eq!(wrap("alpha beta", 3), vec!["alpha beta".to_string()]);
    }

    #[test]
    fn prompt_theme_renders_contextcode_picker_rows() {
        let theme = PromptTheme::new(Ui::plain(80));
        let mut out = String::new();
        theme
            .format_select_prompt(&mut out, "Pick a workspace:")
            .unwrap();
        assert_eq!(out, "  ❯ Pick a workspace   ↑/↓ move · Enter choose");

        let mut out = String::new();
        theme
            .format_select_prompt_item(&mut out, "Personal", true)
            .unwrap();
        assert_eq!(out, "   › Personal ");
        let mut out = String::new();
        theme
            .format_select_prompt_item(&mut out, "Team", false)
            .unwrap();
        assert_eq!(out, "     Team");

        let mut out = String::new();
        theme
            .format_select_prompt_selection(&mut out, "Pick a workspace:", "Personal")
            .unwrap();
        assert_eq!(out, "  ✓ Pick a workspace · Personal");

        let mut out = String::new();
        theme
            .format_multi_select_prompt_item(&mut out, "Cursor", true, false)
            .unwrap();
        assert_eq!(out, "    ● Cursor");

        let mut out = String::new();
        theme
            .format_password_prompt_selection(&mut out, "API key")
            .unwrap();
        assert_eq!(out, "  ✓ API key · ••••••••");
    }

    #[test]
    fn active_row_uses_the_selection_fill_on_a_known_canvas() {
        let theme = PromptTheme::new(Ui::fixed(ColorDepth::TrueColor, Variant::Dark, true, 80));
        let mut out = String::new();
        theme
            .format_select_prompt_item(&mut out, "Personal", true)
            .unwrap();
        assert!(out.contains("48;2;28;45;77"), "{out:?}");
    }

    #[test]
    fn status_bar_joins_segments() {
        let ui = Ui::plain(80);
        let bar = ui.status_bar(
            "ready",
            &["2 editors".into(), "main ✓".into()],
            "restart editor",
        );
        assert!(bar.starts_with("  [READY] │ 2 editors │ main ✓"), "{bar}");
        assert!(bar.ends_with("restart editor"));
    }
}
