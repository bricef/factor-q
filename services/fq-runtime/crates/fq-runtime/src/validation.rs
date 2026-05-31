//! A pluggable validation seam for values that cross a trust
//! boundary.
//!
//! Introduced for MCP server-initiated calls (ADR-0018): the
//! inbound context / schema a server sends *into* our model, and
//! the result we send back *out* to the server, each pass through
//! an ordered chain of validators. The types are deliberately
//! generic — the same seam is reused for sampling results,
//! elicitation inputs and outputs, advertised roots, and any future
//! input/output gate — so nothing here is MCP-specific.
//!
//! A validator is a small, composable stage: it inspects a value
//! and either lets it through unchanged ([`ValidatorResult::Allow`]),
//! replaces it with a transformed value ([`ValidatorResult::Modify`],
//! e.g. a redactor), or rejects it ([`ValidatorResult::Deny`]). A
//! [`ValidatorChain`] applies stages left to right; `Deny`
//! short-circuits. An empty chain (the default) allows everything.

/// The outcome of validating a single value of type `T`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatorResult<T> {
    /// Pass the value through unchanged.
    Allow,
    /// Replace the value with this transformed one (e.g. redacted).
    Modify(T),
    /// Reject the value; the string is a human-readable reason.
    Deny(String),
}

/// A single validation / transformation stage over a `T`.
///
/// `Send + Sync` because validators live in the runner and the MCP
/// handler, held across `await` points and shared between tasks.
pub trait Validator<T>: Send + Sync {
    fn validate(&self, value: &T) -> ValidatorResult<T>;
}

/// An explicit no-op validator: always [`ValidatorResult::Allow`].
/// The default seam is an empty [`ValidatorChain`], which behaves
/// identically; `DefaultAllow` exists for call sites that want to
/// state "no validation" explicitly.
pub struct DefaultAllow;

impl<T> Validator<T> for DefaultAllow {
    fn validate(&self, _value: &T) -> ValidatorResult<T> {
        ValidatorResult::Allow
    }
}

/// An ordered chain of validators applied left to right.
///
/// `Allow` carries the current value forward unchanged; `Modify`
/// carries the transformed value forward to the next stage; `Deny`
/// short-circuits the chain (later stages do not run). An empty
/// chain allows everything.
pub struct ValidatorChain<T> {
    validators: Vec<Box<dyn Validator<T>>>,
}

impl<T> ValidatorChain<T> {
    /// An empty chain — allows everything.
    pub fn new() -> Self {
        Self {
            validators: Vec::new(),
        }
    }

    /// Append a validator (builder style).
    pub fn with(mut self, validator: Box<dyn Validator<T>>) -> Self {
        self.validators.push(validator);
        self
    }

    /// Append a validator in place.
    pub fn push(&mut self, validator: Box<dyn Validator<T>>) {
        self.validators.push(validator);
    }

    /// Whether the chain has no stages (the default, allow-everything).
    pub fn is_empty(&self) -> bool {
        self.validators.is_empty()
    }

    /// Apply every stage to `value`. Returns the final (possibly
    /// transformed) value, or `Err(reason)` on the first `Deny`.
    pub fn run(&self, mut value: T) -> Result<T, String> {
        for validator in &self.validators {
            match validator.validate(&value) {
                ValidatorResult::Allow => {}
                ValidatorResult::Modify(next) => value = next,
                ValidatorResult::Deny(reason) => return Err(reason),
            }
        }
        Ok(value)
    }
}

impl<T> Default for ValidatorChain<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replaces every occurrence of `needle` with `***` via `Modify`,
    /// or `Allow`s when the needle is absent.
    struct Redact {
        needle: &'static str,
    }
    impl Validator<String> for Redact {
        fn validate(&self, value: &String) -> ValidatorResult<String> {
            if value.contains(self.needle) {
                ValidatorResult::Modify(value.replace(self.needle, "***"))
            } else {
                ValidatorResult::Allow
            }
        }
    }

    /// Denies any value containing `marker`.
    struct DenyIf {
        marker: &'static str,
    }
    impl Validator<String> for DenyIf {
        fn validate(&self, value: &String) -> ValidatorResult<String> {
            if value.contains(self.marker) {
                ValidatorResult::Deny(format!("contains {:?}", self.marker))
            } else {
                ValidatorResult::Allow
            }
        }
    }

    /// Panics if ever called — used to prove short-circuiting.
    struct NeverRuns;
    impl Validator<String> for NeverRuns {
        fn validate(&self, _value: &String) -> ValidatorResult<String> {
            panic!("validator after a Deny must not run");
        }
    }

    #[test]
    fn empty_chain_allows_everything() {
        let chain = ValidatorChain::<String>::new();
        assert!(chain.is_empty());
        assert_eq!(chain.run("hello".to_string()), Ok("hello".to_string()));
    }

    #[test]
    fn default_allow_passes_through() {
        let chain = ValidatorChain::new().with(Box::new(DefaultAllow));
        assert_eq!(chain.run("hello".to_string()), Ok("hello".to_string()));
    }

    #[test]
    fn modify_carries_transformed_value_forward() {
        // First stage redacts "secret"; second redacts "key". The
        // second must see the output of the first.
        let chain = ValidatorChain::new()
            .with(Box::new(Redact { needle: "secret" }))
            .with(Box::new(Redact { needle: "key" }));
        assert_eq!(
            chain.run("my secret key".to_string()),
            Ok("my *** ***".to_string())
        );
    }

    #[test]
    fn deny_short_circuits_remaining_stages() {
        let chain = ValidatorChain::new()
            .with(Box::new(DenyIf { marker: "nope" }))
            .with(Box::new(NeverRuns));
        assert_eq!(
            chain.run("a nope b".to_string()),
            Err("contains \"nope\"".to_string())
        );
    }

    #[test]
    fn allow_then_modify_then_allow() {
        let chain = ValidatorChain::new()
            .with(Box::new(DefaultAllow))
            .with(Box::new(Redact { needle: "x" }))
            .with(Box::new(DefaultAllow));
        assert_eq!(chain.run("x y x".to_string()), Ok("*** y ***".to_string()));
    }
}
