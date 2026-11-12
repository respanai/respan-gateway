/// Token counts a provider reports, as an increment or a total. A count of
/// 0 is one the provider did not report. The cache and reasoning counts are
/// parts of the input and output counts, not additions to them. Money
/// belongs in the application's own decimal type. Addition saturates, so it
/// never overflows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Usage {
    /// Every input token, cached or not.
    pub input_tokens: u64,
    /// Every output token, reasoning included.
    pub output_tokens: u64,
    /// Of `input_tokens`, those read from a cache.
    pub cached_input_tokens: u64,
    /// Of `input_tokens`, those written to a cache.
    // WARN: only Anthropic and Converse count them; OpenAI's formats and
    // Gemini report none.
    pub cache_write_input_tokens: u64,
    /// Of `output_tokens`, those spent on reasoning.
    pub reasoning_tokens: u64,
}

impl Usage {
    /// Usage of `input` and `output` tokens, with no cache or reasoning.
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
            ..Self::default()
        }
    }

    /// Whether no usage was reported: every count is 0.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Each count of `self` less that of `other`, at least 0.
    pub fn saturating_sub(&self, other: &Usage) -> Usage {
        self.zip(other, u64::saturating_sub)
    }

    /// Each count of `self` and `other`, combined by `f`.
    fn zip(&self, other: &Usage, f: impl Fn(u64, u64) -> u64) -> Usage {
        Usage {
            input_tokens: f(self.input_tokens, other.input_tokens),
            output_tokens: f(self.output_tokens, other.output_tokens),
            cached_input_tokens: f(self.cached_input_tokens, other.cached_input_tokens),
            cache_write_input_tokens: f(
                self.cache_write_input_tokens,
                other.cache_write_input_tokens,
            ),
            reasoning_tokens: f(self.reasoning_tokens, other.reasoning_tokens),
        }
    }
}

impl std::ops::AddAssign<&Usage> for Usage {
    fn add_assign(&mut self, other: &Usage) {
        *self = self.zip(other, u64::saturating_add);
    }
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, other: Usage) {
        *self += &other;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addition_saturates() {
        let mut u = Usage::new(u64::MAX, 1);
        u += Usage::new(1, 2);
        assert_eq!(u, Usage::new(u64::MAX, 3));
    }

    #[test]
    fn subtraction_stops_at_zero() {
        let u = Usage::new(5, 1).saturating_sub(&Usage::new(3, 4));
        assert_eq!(u, Usage::new(2, 0));
    }
}
