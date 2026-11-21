//! What one generation of Gemini models takes, as the model's name says.

/// The line of a Gemini model that sets its least thinking budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Line {
    FlashLite,
    Flash,
    Other,
}

/// A Gemini model named by its id, such as `gemini-2.5-flash`: the major
/// version and the line, which set what a call to it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Generation {
    pub(crate) line: Line,
    pub(crate) version: u32,
    lead: Option<char>,
    is_2_0: bool,
    flash_3: bool,
    pro_3_1_preview: bool,
}

impl Generation {
    /// The model an id names, `None` when it has no `gemini-<version>`; the
    /// id is as written, so lowercase it first to read any case.
    pub(crate) fn of(model: &str) -> Option<Generation> {
        let has = |name: &str| model.contains(name);
        let (_, after) = model.split_once("gemini-")?;
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        let version = digits.parse().ok()?;
        let named = model.strip_prefix("models/").unwrap_or(model);
        let line = if has("flash-lite") {
            Line::FlashLite
        } else if has("flash") && !(has("2.5-pro") || has("2-5-pro")) {
            Line::Flash
        } else {
            Line::Other
        };
        Some(Generation {
            line,
            version,
            lead: named
                .strip_prefix("gemini-")
                .and_then(|rest| rest.chars().next()),
            is_2_0: has("gemini-2.0") || has("gemini-2-0"),
            flash_3: has("gemini-3-flash") || has("gemini-3.1-flash"),
            pro_3_1_preview: has("gemini-3.1-pro-preview"),
        })
    }

    /// Whether a call signs the function calls in its history: any model
    /// but those before Gemini 3.
    pub(crate) fn needs_signatures(&self) -> bool {
        !matches!(self.lead, Some('1' | '2'))
    }

    /// Whether the model is before 2.5, which does not think.
    pub(crate) fn older(&self) -> bool {
        self.version < 2 || self.is_2_0
    }

    /// The least thinking budget of a Gemini 2.5 model.
    pub(crate) fn minimal_budget(&self) -> u64 {
        match self.line {
            Line::FlashLite => 512,
            Line::Flash => 1,
            Line::Other => 128,
        }
    }

    /// The least thinking level of a Gemini 3 model: the flash line has
    /// `minimal`.
    pub(crate) fn level_floor(&self) -> &'static str {
        if self.flash_3 { "minimal" } else { "low" }
    }

    /// Whether a Gemini 3 model has the `medium` level.
    pub(crate) fn has_medium(&self) -> bool {
        self.flash_3 || self.pro_3_1_preview
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_id_decides_what_its_calls_take() {
        // (id, signs, (version, older, minimal budget, level floor, medium))
        type Takes = Option<(u32, bool, u64, &'static str, bool)>;
        let table: &[(&str, bool, Takes)] = &[
            ("gemini-1.5-pro", false, Some((1, true, 128, "low", false))),
            ("gemini-2.0-flash", false, Some((2, true, 1, "low", false))),
            (
                "gemini-2-0-flash-lite",
                false,
                Some((2, true, 512, "low", false)),
            ),
            ("gemini-2.5-pro", false, Some((2, false, 128, "low", false))),
            ("gemini-2-5-pro", false, Some((2, false, 128, "low", false))),
            ("gemini-2.5-flash", false, Some((2, false, 1, "low", false))),
            (
                "gemini-2.5-flash-lite",
                false,
                Some((2, false, 512, "low", false)),
            ),
            (
                "gemini-3-pro-preview",
                true,
                Some((3, false, 128, "low", false)),
            ),
            ("gemini-3-flash", true, Some((3, false, 1, "minimal", true))),
            (
                "gemini-3.1-flash-lite",
                true,
                Some((3, false, 512, "minimal", true)),
            ),
            (
                "gemini-3.1-pro-preview",
                true,
                Some((3, false, 128, "low", true)),
            ),
            ("gemini-3.1-pro", true, Some((3, false, 128, "low", false))),
            (
                "models/gemini-3-flash",
                true,
                Some((3, false, 1, "minimal", true)),
            ),
            (
                "models/gemini-2.5-flash",
                false,
                Some((2, false, 1, "low", false)),
            ),
            (
                "google/gemini-2.5-pro",
                true,
                Some((2, false, 128, "low", false)),
            ),
            ("gemini-10-pro", false, Some((10, false, 128, "low", false))),
            ("gemini-4-pro", true, Some((4, false, 128, "low", false))),
            ("gemini-pro", true, None),
            ("gemma-3-27b", true, None),
            ("learnlm-x", true, None),
            ("GEMINI-2.5-PRO", true, None),
            ("", true, None),
        ];
        for (id, signs, takes) in table {
            let gemini = Generation::of(id);
            let got = gemini.map(|g| {
                let (v, o) = (g.version, g.older());
                (v, o, g.minimal_budget(), g.level_floor(), g.has_medium())
            });
            assert_eq!(got, *takes, "{id}");
            let signs_got = Generation::of(id).is_none_or(|g| g.needs_signatures());
            assert_eq!(signs_got, *signs, "{id}");
        }
    }
}
