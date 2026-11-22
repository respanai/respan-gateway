//! What one generation of Gemini models takes, as the model's id says.

/// A line of Gemini models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    FlashLite,
    Flash,
    Other,
}

/// How a Gemini model thinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Thinks {
    /// To a level: Gemini 3 and later.
    ToLevel,
    /// To a token budget: Gemini 2.5.
    ToBudget,
    /// Not at all: before Gemini 2.5.
    Never,
}

/// A Gemini model's generation, as its id says: its line and its version,
/// as `gemini-2.5-flash` is Flash 2.5. An id that opens with its line and
/// says no version, such as `gemini-flash-latest`, is the latest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Generation {
    line: Line,
    version: Option<(u32, u32)>,
}

impl Generation {
    /// The generation of the Gemini `model` is, or `None` if it is another
    /// model or of a line with versions of its own, such as
    /// `gemini-nano-banana-2.1`.
    pub(crate) fn of(model: &str) -> Option<Generation> {
        let model = model.to_ascii_lowercase();
        let (_, name) = model.split_once("gemini-")?;
        let line = if name.contains("flash-lite") {
            Line::FlashLite
        } else if name.contains("flash") {
            Line::Flash
        } else {
            Line::Other
        };
        let version = number(name).map(|(major, rest)| {
            let minor = rest.strip_prefix(['.', '-']).and_then(number);
            (major, minor.map_or(0, |(minor, _)| minor))
        });
        let alias = ["flash", "pro"].iter().any(|line| name.starts_with(line));
        if version.is_none() && !alias {
            return None;
        }
        Some(Generation { line, version })
    }

    /// Whether it is of `version` or later, as the latest is.
    fn since(&self, version: (u32, u32)) -> bool {
        self.version.is_none_or(|v| v >= version)
    }

    /// How it thinks.
    pub(crate) fn thinks(&self) -> Thinks {
        if self.since((3, 0)) {
            Thinks::ToLevel
        } else if self.since((2, 5)) {
            Thinks::ToBudget
        } else {
            Thinks::Never
        }
    }

    /// Whether it has the `minimal` level: the flash lines of Gemini 3 up to
    /// 3.7.
    pub(crate) fn has_minimal(&self) -> bool {
        self.line != Line::Other && self.since((3, 0)) && !self.since((3, 7))
    }

    /// The least thinking budget of a Gemini 2.5 model.
    pub(crate) fn minimal_budget(&self) -> u64 {
        match self.line {
            Line::FlashLite => 512,
            Line::Flash => 1,
            Line::Other => 128,
        }
    }

    /// Whether a call signs the function calls in its history: Gemini 3 and
    /// later.
    pub(crate) fn needs_signatures(&self) -> bool {
        self.since((3, 0))
    }
}

/// The version number that opens `s` and what follows it: one or two digits.
fn number(s: &str) -> Option<(u32, &str)> {
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let n = s[..digits].parse::<u32>().ok().filter(|_| digits < 3)?;
    Some((n, &s[digits..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gemini_takes_what_its_id_says() {
        use Thinks::{Never as N, ToBudget as B, ToLevel as L};
        // id, how it thinks, whether it has `minimal`, whether it signs calls
        let table = [
            ("gemini-1.5-pro", N, false, false),
            ("gemini-2.0-flash", N, false, false),
            ("gemini-2-0-flash-lite", N, false, false),
            ("gemini-2.5-pro", B, false, false),
            ("gemini-2-5-pro", B, false, false),
            ("gemini-2.5-flash-preview-09-2025", B, false, false),
            ("models/gemini-2.5-flash-lite", B, false, false),
            ("google/gemini-2.5-pro", B, false, false),
            ("GEMINI-2.5-PRO", B, false, false),
            ("gemini-3-flash-preview", L, true, true),
            ("gemini-3.1-flash-lite", L, true, true),
            ("gemini-3.1-pro-preview", L, false, true),
            ("gemini-3.5-flash-lite", L, true, true),
            ("gemini-3.6-flash", L, true, true),
            ("gemini-3.8-flash", L, false, true),
            ("gemini-4-carbon", L, false, true),
            ("gemini-4-neon", L, false, true),
            ("gemini-10-pro", L, false, true),
            ("gemini-flash-latest", L, false, true),
        ];
        for (id, thinks, minimal, signs) in table {
            let gemini = Generation::of(id).expect(id);
            assert_eq!(gemini.thinks(), thinks, "{id} thinks");
            assert_eq!(gemini.has_minimal(), minimal, "{id} has minimal");
            assert_eq!(gemini.needs_signatures(), signs, "{id} signs");
        }
        let budget = |id| Generation::of(id).expect(id).minimal_budget();
        assert_eq!(budget("gemini-2.5-flash-lite"), 512);
        assert_eq!(budget("gemini-2.5-flash"), 1);
        assert_eq!(budget("gemini-2.5-pro"), 128);
        assert_eq!(Generation::of("gemma-3-27b"), None);
    }
}
