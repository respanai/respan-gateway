//! The gateway's log lines on stderr: a timestamp, a level and a message,
//! as text or as one JSON object a line.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::SystemTime;

use clap::ValueEnum;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::requests::rfc3339;

/// How log lines are written. `off` silences the request log alone:
/// warnings are always written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Text,
    Json,
    Off,
}

/// Color policy for human-readable output. JSON is always unstyled.
#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum Color {
    #[default]
    Auto,
    Always,
    Never,
}

static COLOR: AtomicU8 = AtomicU8::new(0);

impl Color {
    pub(crate) fn apply(self) {
        COLOR.store(self as u8, Ordering::Relaxed);
    }
}

fn color_enabled(terminal: bool) -> bool {
    match COLOR.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            terminal
                && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
                && std::env::var_os("TERM").is_none_or(|v| v != "dumb")
        }
    }
}

pub(crate) fn paint(text: &str, style: &str, stderr: bool) -> String {
    let terminal = if stderr {
        std::io::stderr().is_terminal()
    } else {
        std::io::stdout().is_terminal()
    };
    if color_enabled(terminal) {
        format!("\x1b[{style}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// Style the CLI's aligned reports without changing their text or spacing.
pub(crate) fn report(text: &str) -> String {
    if !color_enabled(std::io::stdout().is_terminal()) {
        return text.to_owned();
    }
    let style = |text: &str, code: &str| format!("\x1b[{code}m{text}\x1b[0m");
    let mut section = "";
    text.split('\n')
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                return match line.split_once(" listening on ") {
                    Some((title, address)) => format!(
                        "{} listening on {}",
                        style(title, "1;36"),
                        style(address, "1;32")
                    ),
                    None => style(line, "1;36"),
                };
            }
            // Reports use two spaces, a ten-column label, then a space.
            let Some(prefix) = line
                .get(..13)
                .filter(|p| p.starts_with("  ") && p.is_ascii())
            else {
                return line.to_owned();
            };
            let label = prefix.trim();
            if !label.is_empty() {
                section = label;
            }
            let value = &line[13..];
            let value = match section {
                "auth" => style(value, if value.starts_with("off") { "33" } else { "32" }),
                "listen" => style(value, "1;32"),
                "web ui" if value.starts_with("http") => style(value, "4;36"),
                "web ui" => style(value, if value == "off" { "33" } else { "32" }),
                "config" | "try" => style(value, "2"),
                "endpoints" => match value.split_once(' ') {
                    Some((method, path)) => format!("{} {path}", style(method, "32")),
                    None => value.to_owned(),
                },
                "providers" => match value.split_once("  ") {
                    Some((name, formats)) => {
                        format!("{}  {}", style(name, "1"), style(formats, "2"))
                    }
                    None => value.to_owned(),
                },
                _ => value.to_owned(),
            };
            format!("{}{value}", style(prefix, "36"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `text` with its control characters escaped, so none can forge a line
/// or drive a terminal.
pub(crate) fn escape_controls(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

impl LogFormat {
    /// The line for `message` and its `fields`.
    pub fn line(self, level: Level, message: &str, fields: Map<String, Value>) -> String {
        let ts = rfc3339(SystemTime::now());
        match self {
            LogFormat::Json => {
                let mut line = fields;
                line.insert("ts".into(), json!(ts));
                line.insert("level".into(), json!(level.label()));
                line.insert("msg".into(), json!(message));
                Value::Object(line).to_string()
            }
            _ => self.tagged(
                level,
                &level.label().to_ascii_uppercase(),
                &escape_controls(message),
            ),
        }
    }

    /// A text line: the time, `tag` (a level, or a request's status) in the
    /// level's color, and `message`, already escaped and styled.
    pub fn tagged(self, level: Level, tag: &str, message: &str) -> String {
        let style = match level {
            Level::Info => "32",
            Level::Warn => "33",
            Level::Error => "31",
        };
        let ts = rfc3339(SystemTime::now());
        let time = ts.split_once('T').map_or(ts.as_str(), |(_, time)| time);
        format!(
            "{} {} {message}",
            paint(time, "2", true),
            paint(&format!("{tag:<5}"), style, true)
        )
    }

    /// Writes a message of the gateway's own to stderr.
    pub fn say(self, level: Level, message: &str) {
        eprintln!("{}", self.line(level, message, Map::new()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_cannot_inject_terminal_controls_or_json_metadata() {
        let text = LogFormat::Text.line(Level::Error, "bad\nforged\u{1b}[2J", Map::new());
        assert!(!text.contains('\n') && !text.contains('\u{1b}'));
        let fields = json!({"level": "fake", "msg": "fake", "status": 502})
            .as_object()
            .expect("fields")
            .clone();
        let line = LogFormat::Json.line(Level::Error, "request", fields);
        let parsed: Value = serde_json::from_str(&line).expect("JSON");
        assert_eq!(parsed["level"], "error");
        assert_eq!(parsed["msg"], "request");
        assert_eq!(parsed["status"], 502);
    }

    #[test]
    fn a_line_is_text_or_json() {
        let text = LogFormat::Text.line(Level::Warn, "careful", Map::new());
        assert!(text.contains(" WARN  careful"), "{text}");
        let json = LogFormat::Json.line(Level::Info, "hi", Map::new());
        let json: Value = serde_json::from_str(&json).unwrap_or_default();
        assert_eq!(json["level"], "info");
        assert_eq!(json["msg"], "hi");
        assert!(json["ts"].as_str().is_some_and(|ts| ts.ends_with('Z')));
    }
}
