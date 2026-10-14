//! Terminal output: human text, progress bars and QR codes, or one JSON object per line.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use nuee_proto::EndpointId;
use serde_json::{Map, Value, json};

use crate::{exit, fail, qr};

/// Where and how to print.
#[derive(Debug, Clone)]
pub struct Ui {
    json: bool,
    quiet: bool,
    color: bool,
    bars: bool,
}

/// Mark of a passed check.
pub const OK: &str = "✓";
/// Mark of a failed check.
pub const KO: &str = "✗";

impl Ui {
    /// Output settings from the global flags, `NO_COLOR` and whether stdout/stderr are
    /// terminals.
    pub fn new(json: bool, quiet: bool) -> Ui {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let out_tty = std::io::stdout().is_terminal();
        let err_tty = std::io::stderr().is_terminal();
        Ui {
            json,
            quiet,
            color: !json && !no_color && out_tty,
            bars: !json && !quiet && err_tty,
        }
    }

    /// NDJSON mode.
    pub fn is_json(&self) -> bool {
        self.json
    }

    /// Emits a JSON event (NDJSON mode only): `{"event": name, ...fields}`.
    pub fn emit(&self, event: &str, fields: Value) {
        if !self.json {
            return;
        }
        let mut obj = Map::new();
        obj.insert("event".into(), Value::String(event.into()));
        if let Value::Object(m) = fields {
            obj.extend(m);
        }
        let line = clean_json(&Value::Object(obj).to_string());
        let mut out = std::io::stdout().lock();
        if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
            tracing::debug!("stdout closed");
        }
    }

    /// Primary output: stdout in human mode, stderr in NDJSON mode.
    pub fn say(&self, text: &str) {
        if self.json {
            if !self.quiet {
                eprintln!("{text}");
            }
        } else {
            println!("{text}");
        }
    }

    /// Secondary information on stderr, hidden by `--quiet`.
    pub fn note(&self, text: &str) {
        if !self.quiet {
            eprintln!("{text}");
        }
    }

    /// Reports an error: an `error` event in NDJSON mode, one line on stderr otherwise.
    pub fn error(&self, code: u8, message: &str) {
        if self.json {
            self.emit("error", json!({ "code": code, "message": message }));
        }
        let prefix = self.paint("error:", "31");
        // Errors can quote what a peer sent; line breaks are kept, other controls escaped.
        let shown: Vec<_> = message.split('\n').map(clean).collect();
        eprintln!("{prefix} {}", shown.join("\n"));
    }

    /// Wraps `text` in an ANSI color when colors are on.
    pub fn paint(&self, text: &str, sgr: &str) -> String {
        if self.color {
            format!("\x1b[{sgr}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    /// A check line: `✓ name: detail` or `✗ name: detail (hint)`.
    pub fn check(&self, name: &str, ok: bool, detail: &str, hint: Option<&str>) {
        self.emit(
            "check",
            json!({ "name": name, "ok": ok, "detail": detail, "hint": hint }),
        );
        let mark = if ok {
            self.paint(OK, "32")
        } else {
            self.paint(KO, "31")
        };
        let mut line = format!("{mark} {name}: {detail}");
        if let Some(h) = hint
            && !ok
        {
            line.push_str(&format!(" ({h})"));
        }
        self.say(&line);
    }

    /// Shows a link once the endpoint is online: QR code, the full link and, for swarms,
    /// the coordinator fingerprint.
    pub fn show_link(
        &self,
        kind: &str,
        link: &str,
        fingerprint_of: Option<&EndpointId>,
        qr_svg: Option<&Path>,
    ) -> anyhow::Result<()> {
        if let Some(path) = qr_svg {
            let doc = qr::svg(link)?;
            std::fs::write(path, doc)
                .map_err(|e| fail(exit::ERROR, format!("cannot write {}: {e}", path.display())))?;
        }
        let fp = fingerprint_of.map(nuee_proto::fingerprint);
        self.emit(
            "link",
            json!({ "kind": kind, "link": link, "fingerprint": fp.map(|f| f.to_vec()) }),
        );
        if self.json {
            return Ok(());
        }
        if !self.quiet {
            match qr::terminal(link) {
                Ok(code) => println!("{code}"),
                Err(e) => eprintln!("(no QR code: {e:#})"),
            }
        }
        println!("{link}");
        if let Some(fp) = fp {
            println!(
                "fingerprint {}  (compare with the screen of whoever joins)",
                fp.join(" ")
            );
        }
        if let Some(path) = qr_svg {
            self.note(&format!("QR code written to {}", path.display()));
        }
        Ok(())
    }

    /// A progress bar on stderr, hidden in NDJSON mode, with `--quiet` or without a terminal.
    pub fn bar(&self, total: u64, unit: &str) -> ProgressBar {
        if !self.bars {
            return ProgressBar::hidden();
        }
        let bar = ProgressBar::with_draw_target(Some(total), ProgressDrawTarget::stderr());
        let template = format!("{{bar:30}} {{pos}}/{{len}} {unit} {{msg}}");
        match ProgressStyle::with_template(&template) {
            Ok(style) => bar.set_style(style),
            Err(e) => tracing::debug!("progress template: {e}"),
        }
        bar.enable_steady_tick(Duration::from_millis(200));
        bar
    }

    /// A line printed above `bar`. A hidden bar (no terminal, `--quiet`, NDJSON) drops its
    /// `println`, so the line then goes to stderr directly unless in NDJSON mode, where the
    /// matching event already carries it; `--quiet` still hides it.
    pub fn bar_line(&self, bar: &ProgressBar, text: &str) {
        if bar.is_hidden() {
            if !self.json {
                self.note(text);
            }
        } else {
            bar.println(text);
        }
    }

    /// Asks a yes/no question on the terminal; anything but `y`/`yes` is no.
    pub fn confirm(&self, question: &str) -> anyhow::Result<bool> {
        if !std::io::stdin().is_terminal() {
            return Err(fail(
                exit::USAGE,
                "manual approval needs an interactive terminal",
            ));
        }
        eprint!("{question} [y/N] ");
        std::io::stderr().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let answer = line.trim().to_ascii_lowercase();
        Ok(answer == "y" || answer == "yes")
    }
}

/// Characters a peer could use to move the cursor, rewrite or hide what the terminal shows:
/// C0/C1 controls, DEL, and bidirectional overrides and isolates.
fn unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

/// A string another machine chose (a job name, a coordinator's reason, an archive name), made
/// safe to print: every control or bidi character becomes `\u{..}`.
pub fn clean(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.chars().any(unsafe_char) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if unsafe_char(c) {
            out.push_str(&format!("\\u{{{:x}}}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Like [`clean`] for a JSON text: the characters become `\uXXXX` escapes, so the line stays
/// valid JSON with the same meaning (they can only occur inside strings).
pub fn clean_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if unsafe_char(c) {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_escapes_every_terminal_control() {
        let evil = "job\u{1b}[2K\r\u{1b}[1Aok\u{7}\u{7f}\u{9b}31m\u{202e}gpj.exe\n";
        let shown = clean(evil);
        assert!(!shown.chars().any(unsafe_char), "{shown:?}");
        assert_eq!(
            shown,
            "job\\u{1b}[2K\\u{d}\\u{1b}[1Aok\\u{7}\\u{7f}\\u{9b}31m\\u{202e}gpj.exe\\u{a}"
        );
        assert!(matches!(clean("café ✓ 🐝"), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn clean_json_stays_the_same_json() {
        let v = json!({ "argv": ["a\u{9b}b", "\u{7f}", "\u{202e}x", "\u{1b}"] });
        let raw = v.to_string();
        assert!(raw.contains('\u{9b}'), "serde_json leaves C1 controls raw");
        let line = clean_json(&raw);
        assert!(!line.chars().any(unsafe_char), "{line}");
        let back: Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(back, v);
    }
}
