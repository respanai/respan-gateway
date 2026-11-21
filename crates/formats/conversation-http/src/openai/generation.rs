//! What one generation of OpenAI's models takes, the same at every vendor
//! that serves them.

/// The models only Responses serves, by a part of their names.
const RESPONSES_ONLY: &[&str] = &["-pro", "codex", "deep-research", "computer-use"];

/// An OpenAI model's generation, as its id says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Generation {
    /// Only Responses serves it, by a part of its name.
    responses_only: bool,
    /// The `(major, minor)` of a `gpt-` id, if it names one.
    version: Option<(u32, u32)>,
}

impl Generation {
    /// The generation of `model`, an id as OpenAI names it.
    pub fn of(model: &str) -> Generation {
        let responses_only = RESPONSES_ONLY.iter().any(|p| model.contains(p));
        let version = model.strip_prefix("gpt-").and_then(|version| {
            let version = version.split('-').next().unwrap_or_default();
            let (major, minor) = version.split_once('.').unwrap_or((version, "0"));
            Some((major.parse().ok()?, minor.parse().ok()?))
        });
        Generation {
            responses_only,
            version,
        }
    }

    /// Whether a chat goes to Responses: a model only it serves, or, if it
    /// offers `tools`, gpt-5.4 or later, whose reasoning Chat Completions
    /// refuses with function tools (from gpt-5.6 they reason unless told not
    /// to). Any other chat stays in Chat Completions, which takes what
    /// Responses cannot, such as `stop`, `seed` and `n`.
    pub fn in_responses(self, tools: bool) -> bool {
        self.responses_only || (tools && self.version.is_some_and(|v| v >= (5, 4)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chat_goes_to_responses_for_a_model_only_it_serves_or_tools_it_refuses() {
        let in_responses = |model: &str, tools| Generation::of(model).in_responses(tools);
        for model in [
            "gpt-5.5-pro",
            "gpt-5-codex",
            "o3-deep-research",
            "computer-use-preview",
        ] {
            assert!(in_responses(model, false), "{model}");
        }
        for model in ["gpt-5.4", "gpt-5.6-terra", "gpt-6-luna"] {
            assert!(in_responses(model, true), "{model}");
            assert!(!in_responses(model, false), "{model} without tools");
        }
        for model in ["gpt-5.2", "gpt-5-mini", "gpt-4o", "o4-mini"] {
            assert!(!in_responses(model, true), "{model}");
        }
    }

    #[test]
    fn a_generation_is_read_from_the_id_as_written() {
        let in_responses = |model: &str, tools| Generation::of(model).in_responses(tools);
        assert!(in_responses("gpt-5.4-mini", true) && !in_responses("gpt-5.4-mini", false));
        assert!(in_responses("gpt-5.10", true) && !in_responses("gpt-5.10", false));
        assert!(!in_responses("gpt-4.1", true));
        assert!(!in_responses("GPT-5.4", true));
        assert!(!in_responses("openai/gpt-5.4", true));
    }
}
