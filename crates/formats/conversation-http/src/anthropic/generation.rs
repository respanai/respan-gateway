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

/// A Claude model's generation, as its lowercase id says: the name of the
/// model, a dated id, a cloud's id, or an alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Generation {
    thinks: Thinks,
    forces_tools: bool,
    disables_thinking: bool,
}

impl Generation {
    /// The generation of the Claude `model` is, or `None` if it is another
    /// model.
    pub(crate) fn of(model: &str) -> Option<Generation> {
        model.contains("claude").then(|| Generation {
            thinks: thinks(model),
            forces_tools: forces_tools(model),
            disables_thinking: disables_thinking(model),
        })
    }

    /// How it thinks.
    pub(crate) fn thinks(&self) -> Thinks {
        self.thinks
    }

    /// Whether it takes a forced tool choice: Opus and Sonnet 5.5, and Fable
    /// and Mythos 5.1, and later refuse one.
    pub(crate) fn forces_tools(&self) -> bool {
        self.forces_tools
    }

    /// Whether it takes `thinking: {type: "disabled"}`: Opus and Sonnet 5.5,
    /// Fable and Mythos, and later refuse it.
    pub(crate) fn disables_thinking(&self) -> bool {
        self.disables_thinking
    }
}

fn thinks(model: &str) -> Thinks {
    let budgeted = [
        "-4-0", "-4.0", "-4-1", "-4.1", "-4-5", "-4.5", "-4-2025", "-4@",
    ];
    let family = |family: &str| {
        model.ends_with(&format!("{family}-4"))
            || budgeted
                .iter()
                .any(|v| model.contains(&format!("{family}{v}")))
    };
    if model.contains("claude-3-7") || model.contains("3-7-sonnet") {
        Thinks::ToBudget
    } else if model.contains("claude-3") {
        Thinks::Never
    } else if ["opus", "sonnet", "haiku"].iter().any(|f| family(f)) {
        Thinks::ToBudget
    } else {
        Thinks::Adaptively
    }
}

/// Whether Claude `model` is of a `family` at `from` or later, as
/// `claude-opus-5-5` is Opus 5.5; a dated model is of its major's `.0`.
fn at_least(model: &str, (family, from): &(&str, (u32, u32))) -> bool {
    fn number(s: &str) -> Option<(u32, &str)> {
        let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let n = s[..digits].parse::<u32>().ok().filter(|_| digits < 3)?;
        Some((n, &s[digits..]))
    }
    let Some((major, rest)) = model
        .split_once(&format!("{family}-"))
        .and_then(|(_, r)| number(r))
    else {
        return false;
    };
    let minor = rest.strip_prefix(['-', '.']).and_then(number);
    (major, minor.map_or(0, |(minor, _)| minor)) >= *from
}

fn forces_tools(model: &str) -> bool {
    let refusing = [
        ("opus", (5, 5)),
        ("sonnet", (5, 5)),
        ("fable", (5, 1)),
        ("mythos", (5, 1)),
    ];
    !refusing.iter().any(|f| at_least(model, f))
}

fn disables_thinking(model: &str) -> bool {
    let refusing = [
        ("opus", (5, 5)),
        ("sonnet", (5, 5)),
        ("fable", (5, 0)),
        ("mythos", (5, 0)),
    ];
    !refusing.iter().any(|f| at_least(model, f))
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
