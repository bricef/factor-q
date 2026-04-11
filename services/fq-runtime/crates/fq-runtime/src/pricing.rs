//! Static model pricing table.
//!
//! Used for cost calculation on the fast path, where reaching out to an
//! external pricing service would add latency to every LLM call. When a
//! new model ships, add its entry here and the runtime will report costs
//! for it immediately.
//!
//! Prices are USD per million tokens and taken from public pricing pages
//! at the time of writing. They will drift over time and should be
//! reviewed periodically.

/// Per-model input and output prices in USD per million tokens.
#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
}

impl ModelPricing {
    /// Look up the pricing for a given model identifier. Returns `None`
    /// for unknown models; the caller is expected to treat unknown cost
    /// as $0 and emit a warning.
    pub fn lookup(model: &str) -> Option<Self> {
        // Normalise the model name a little so aliases like "claude-haiku"
        // hit the same entry as the canonical identifier.
        match model {
            // Claude Haiku family
            "claude-haiku"
            | "claude-haiku-4-5"
            | "claude-haiku-4-5-20251001"
            | "claude-3-5-haiku-latest"
            | "claude-3-5-haiku-20241022" => Some(Self {
                input_per_million: 1.00,
                output_per_million: 5.00,
            }),

            // Claude Sonnet family
            "claude-sonnet"
            | "claude-sonnet-4-6"
            | "claude-3-5-sonnet-latest"
            | "claude-3-5-sonnet-20241022" => Some(Self {
                input_per_million: 3.00,
                output_per_million: 15.00,
            }),

            // Claude Opus family
            "claude-opus"
            | "claude-opus-4-6"
            | "claude-opus-4-6[1m]"
            | "claude-3-opus-latest"
            | "claude-3-opus-20240229" => Some(Self {
                input_per_million: 15.00,
                output_per_million: 75.00,
            }),

            // OpenAI family — common models we are likely to hit in tests.
            "gpt-4o" | "gpt-4o-2024-11-20" => Some(Self {
                input_per_million: 2.50,
                output_per_million: 10.00,
            }),
            "gpt-4o-mini" | "gpt-4o-mini-2024-07-18" => Some(Self {
                input_per_million: 0.15,
                output_per_million: 0.60,
            }),

            _ => None,
        }
    }

    /// Calculate the total cost in USD for the given token counts.
    ///
    /// Returns `(input_cost, output_cost, total_cost)`.
    pub fn calculate(&self, input_tokens: u32, output_tokens: u32) -> (f64, f64, f64) {
        let input_cost = (input_tokens as f64) * self.input_per_million / 1_000_000.0;
        let output_cost = (output_tokens as f64) * self.output_per_million / 1_000_000.0;
        (input_cost, output_cost, input_cost + output_cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_models_have_pricing() {
        assert!(ModelPricing::lookup("claude-haiku").is_some());
        assert!(ModelPricing::lookup("claude-sonnet").is_some());
        assert!(ModelPricing::lookup("claude-opus").is_some());
        assert!(ModelPricing::lookup("gpt-4o-mini").is_some());
    }

    #[test]
    fn unknown_models_return_none() {
        assert!(ModelPricing::lookup("imaginary-3000").is_none());
    }

    #[test]
    fn haiku_calculates_correct_cost() {
        let pricing = ModelPricing::lookup("claude-haiku").unwrap();
        // 1M input tokens at $1/M = $1; 1M output tokens at $5/M = $5
        let (input, output, total) = pricing.calculate(1_000_000, 1_000_000);
        assert!((input - 1.00).abs() < 1e-9);
        assert!((output - 5.00).abs() < 1e-9);
        assert!((total - 6.00).abs() < 1e-9);
    }

    #[test]
    fn small_token_counts_produce_fractional_cost() {
        let pricing = ModelPricing::lookup("claude-haiku").unwrap();
        let (_, _, total) = pricing.calculate(100, 200);
        // 100 input @ $1/M = $0.0001; 200 output @ $5/M = $0.001
        assert!((total - 0.0011).abs() < 1e-9);
    }
}
