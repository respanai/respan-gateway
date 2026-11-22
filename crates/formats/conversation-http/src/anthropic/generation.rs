//! What one generation of Claude models takes, as their id says.

/// How a Claude model thinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Thinks {
    /// Adaptively, to an effort: Claude 4.6 and later.
    Adaptively,
    /// To a token budget: Claude 3.7 to 4.5.
    ToBudget,
    /// Not at all: Claude 3 but 3.7.
    Never,
}

/// A line of Claude models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    Opus,
    Sonnet,
    Haiku,
    Fable,
    Mythos,
    Other,
}

/// A Claude model's generation, as its id says: its line and its version,
/// as `claude-opus-5-5` is Opus 5.5, `claude-3-7-sonnet` Sonnet 3.7 and a
/// dated id its major's `.0`. The id is the name of the model, a dated id,
/// a cloud's id, or an alias, in any case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Generation {
    line: Line,
    version: Option<(u32, u32)>,
}

impl Generation {
    /// The generation of the Claude `model` is, or `None` if it is another
    /// model.
    pub(crate) fn of(model: &str) -> Option<Generation> {
        let model = model.to_ascii_lowercase();
        let (_, name) = model.split_once("claude")?;
        let lines = [
            ("opus", Line::Opus),
            ("sonnet", Line::Sonnet),
            ("haiku", Line::Haiku),
            ("fable", Line::Fable),
            ("mythos", Line::Mythos),
        ];
        let named = lines.iter().find_map(|&(word, line)| {
            let (before, after) = name.split_once(word)?;
            Some((line, before, after))
        });
        let (line, before, after) = named.unwrap_or((Line::Other, name, ""));
        // The version follows the line, as in `claude-opus-4-5`, or comes
        // before it, as in `claude-3-5-sonnet`.
        let version = [after, before].into_iter().find_map(|part| {
            let (major, rest) = number(part.strip_prefix('-')?)?;
            let minor = rest.strip_prefix(['-', '.']).and_then(number);
            Some((major, minor.map_or(0, |(minor, _)| minor)))
        });
        Some(Generation { line, version })
    }

    /// How it thinks; a model whose id says no version, as the latest does.
    pub(crate) fn thinks(&self) -> Thinks {
        match self.version {
            Some(v) if v < (3, 7) => Thinks::Never,
            Some(v) if v < (4, 6) => Thinks::ToBudget,
            _ => Thinks::Adaptively,
        }
    }

    /// Whether it takes a forced tool choice: Opus and Sonnet 5.5, and Fable
    /// and Mythos 5.1, and later refuse one.
    pub(crate) fn forces_tools(&self) -> bool {
        !self.since(&[
            (Line::Opus, (5, 5)),
            (Line::Sonnet, (5, 5)),
            (Line::Fable, (5, 1)),
            (Line::Mythos, (5, 1)),
        ])
    }

    /// Whether it takes `thinking: {type: "disabled"}`: Opus and Sonnet 5.5,
    /// Fable and Mythos, and later refuse it.
    pub(crate) fn disables_thinking(&self) -> bool {
        !self.since(&[
            (Line::Opus, (5, 5)),
            (Line::Sonnet, (5, 5)),
            (Line::Fable, (5, 0)),
            (Line::Mythos, (5, 0)),
        ])
    }

    /// Whether it is of the version `from` has for its line, or later; not
    /// a model whose id says no version.
    fn since(&self, from: &[(Line, (u32, u32))]) -> bool {
        from.iter()
            .any(|&(line, at)| line == self.line && self.version.is_some_and(|v| v >= at))
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
    fn a_claude_takes_what_its_id_says() {
        use Thinks::{Adaptively as A, Never as N, ToBudget as B};
        // id, how it thinks, whether it forces a tool, whether it disables thinking
        let table = [
            ("claude-3-opus-20240229", N, true, true),
            ("claude-3-5-sonnet-20241022", N, true, true),
            ("claude-3-5-haiku", N, true, true),
            ("claude-3-7-sonnet", B, true, true),
            ("anthropic.claude-3-7-sonnet-20250219-v1:0", B, true, true),
            ("claude-opus-4", B, true, true),
            ("claude-opus-4-20250514", B, true, true),
            ("claude-opus-4@20250514", B, true, true),
            ("claude-opus-4.1", B, true, true),
            ("claude-sonnet-4-5-20250929", B, true, true),
            (
                "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
                B,
                true,
                true,
            ),
            ("claude-haiku-4-5", B, true, true),
            ("claude-sonnet-4-6", A, true, true),
            ("claude-opus-5", A, true, true),
            ("claude-opus-5-5", A, false, false),
            ("claude-sonnet-5-4", A, true, true),
            ("claude-sonnet-5-5-20260101", A, false, false),
            ("claude-fable-5", A, true, false),
            ("claude-fable-5-1", A, false, false),
            ("claude-mythos-5", A, true, false),
            ("claude-mythos-5-1", A, false, false),
            ("claude-nova-7", A, true, true),
            ("claude-v2", A, true, true),
            ("anthropic/claude-3.7-sonnet", B, true, true),
            ("claude-sonnet-4.5", B, true, true),
            ("CLAUDE-OPUS-5-5", A, false, false),
        ];
        for (id, thinks, forces, disables) in table {
            let claude = Generation::of(id).expect(id);
            assert_eq!(claude.thinks(), thinks, "{id} thinks");
            assert_eq!(claude.forces_tools(), forces, "{id} forces tools");
            assert_eq!(
                claude.disables_thinking(),
                disables,
                "{id} disables thinking"
            );
        }
        assert_eq!(Generation::of("gpt-4o"), None);
    }
}
