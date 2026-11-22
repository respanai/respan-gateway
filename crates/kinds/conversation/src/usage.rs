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

/// Turns a provider's running usage totals into increments.
#[derive(Debug, Default)]
pub struct Totals(Usage);

impl Totals {
    /// What `total` adds to the totals seen so far: usage only grows, and a
    /// part stays within its whole, though providers report more reasoning
    /// than output or count a cache write again as a read.
    pub fn increment(&mut self, total: &Usage) -> Usage {
        let (seen, mut step) = (self.0, total.zip(&self.0, u64::saturating_sub));
        let output = seen.output_tokens.saturating_add(step.output_tokens);
        step.reasoning_tokens = step.reasoning_tokens.min(output - seen.reasoning_tokens);
        let cached = seen.cached_input_tokens + seen.cache_write_input_tokens;
        let mut room = seen.input_tokens.saturating_add(step.input_tokens) - cached;
        step.cached_input_tokens = step.cached_input_tokens.min(room);
        room -= step.cached_input_tokens;
        step.cache_write_input_tokens = step.cache_write_input_tokens.min(room);
        self.0 += &step;
        step
    }
}

/// An increment reported apart from the totals, as seen.
impl std::ops::AddAssign<&Usage> for Totals {
    fn add_assign(&mut self, step: &Usage) {
        self.0 += step;
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
    fn totals_become_increments_whose_parts_fit() {
        let mut totals = Totals::default();
        let total = |n| Usage::new(0, n);
        assert_eq!(totals.increment(&total(1)).output_tokens, 1);
        assert_eq!(totals.increment(&total(3)).output_tokens, 2);
        assert!(totals.increment(&total(3)).is_empty());
        assert!(totals.increment(&total(2)).is_empty(), "never negative");
        let mut cache = Usage::new(10, 3);
        cache.cache_write_input_tokens = 5;
        totals.increment(&cache);
        (cache.cached_input_tokens, cache.cache_write_input_tokens) = (9, 0);
        assert_eq!(totals.increment(&cache).cached_input_tokens, 5);
    }
}
