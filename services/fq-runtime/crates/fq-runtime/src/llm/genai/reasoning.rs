//! Reasoning parts on the wire.
//!
//! The write side of the adapter's reasoning handling: how one recorded
//! part is encoded for the provider — or dropped, and why — and the
//! wrapper this crate mints for a bare continuity token so the event log
//! can tell it from a provider block. The read side lives in
//! `from_provider_response`, next to the rest of the response decode.

use serde_json::Value;

use ::genai as provider;

/// Encode one reasoning part for the wire, or drop it and say why.
///
/// Two reasons a part does not go out, and neither is silent:
///
/// **Model mismatch.** Reasoning is verified or ignored against the model
/// that produced it, so replaying it to a different model is at best
/// wasted input tokens and at worst a protocol violation. Multi-agent
/// graphs have cross-model edges by construction (ADR-0003), so this is
/// an expected drop rather than an error — logged at debug, not warn.
///
/// **No readable text.** `Opaque` reasoning carries its content in a
/// provider token, and genai's `ContentPart::ReasoningContent` is a bare
/// string with nowhere to put one. Anthropic's signed and redacted blocks
/// round-trip through `ContentPart::Custom` instead, which genai's
/// Anthropic adapter echoes verbatim.
///
/// Note what this deliberately does *not* do: branch on the provider. We
/// hand genai the semantic part and let its adapter encode it — the
/// OpenAI-compatible adapter hoists `ReasoningContent` into the sibling
/// `reasoning_content` field, which is exactly the Kimi/DeepSeek
/// round-trip, while the Anthropic adapter omits an unpaired one. That
/// omission is correct: a `Plain` block has no signature, and Anthropic
/// rejects a thinking block that lacks one, so it must never reach that
/// wire.
pub(super) fn encode_reasoning(
    reasoning: crate::events::Reasoning,
    target_model: &str,
) -> Option<provider::chat::ContentPart> {
    if reasoning.model != target_model {
        tracing::debug!(
            produced_by = %reasoning.model,
            target_model = %target_model,
            "dropping reasoning on a cross-model edge; it is tied to the model that produced it"
        );
        return None;
    }
    match reasoning.content {
        // Readable-only reasoning is a semantic part: genai's
        // OpenAI-compatible adapter hoists it into the sibling
        // `reasoning_content` field (the Kimi/DeepSeek round trip), and
        // its Anthropic adapter drops it — correctly, since a block with
        // no signature is one Anthropic would reject.
        crate::events::ReasoningContent::Plain { text } => {
            Some(provider::chat::ContentPart::ReasoningContent(text))
        }
        // Signed reasoning goes back as the provider's own block,
        // verbatim. Anthropic verifies a thinking block *against* its
        // signature, so the block is the unit that can be replayed — not
        // the text, and not the signature alone.
        //
        // `Custom` rather than a provider branch: genai's Anthropic
        // adapter echoes these blocks unchanged and its OpenAI adapter
        // ignores them, so this arm is right for both without asking
        // which one we are talking to — which this layer cannot know.
        // genai also offers a `ThoughtSignature` + `ReasoningContent`
        // pair that its Anthropic adapter rebuilds into a block; sending
        // the block we hold keeps every key the provider put in it.
        crate::events::ReasoningContent::Signed { token, .. } => Some(
            provider::chat::ContentPart::Custom(provider::chat::CustomPart {
                model_iden: None,
                data: token,
            }),
        ),
        // Opaque reasoning is one of two tokens, and the token says which
        // by construction — still not a provider branch, but a branch on
        // what we hold:
        //
        // - A bare continuity token (Gemini's `thoughtSignature`) goes back
        //   as genai's `ThoughtSignature` part, which its Gemini adapter
        //   attaches to the next function call it meets — the part the
        //   token came with, in the order this crate records. A `Custom`
        //   part would be ignored on that wire.
        // - A whole provider block (Anthropic's `redacted_thinking`) goes
        //   back verbatim as `Custom`, like a signed block.
        crate::events::ReasoningContent::Opaque { token } => match bare_signature_of(&token) {
            Some(signature) => Some(provider::chat::ContentPart::ThoughtSignature(
                signature.to_string(),
            )),
            None => Some(provider::chat::ContentPart::Custom(
                provider::chat::CustomPart {
                    model_iden: None,
                    data: token,
                },
            )),
        },
    }
}

/// The token type this crate mints for a continuity token that arrived
/// with no readable text and no surrounding block — Gemini's
/// `thoughtSignature`. The type is ours, so a reader of the event log
/// can tell it from a provider block; the signature is the provider's,
/// verbatim.
pub(super) const BARE_SIGNATURE_TYPE: &str = "thought_signature";

/// An opaque reasoning part for a bare continuity token.
pub(super) fn bare_signature(signature: &str) -> crate::events::ReasoningContent {
    crate::events::ReasoningContent::Opaque {
        token: serde_json::json!({
            "type": BARE_SIGNATURE_TYPE,
            "signature": signature,
        }),
    }
}

/// The signature inside an opaque token minted by [`bare_signature`], or
/// `None` for any other opaque token (a provider block, replayed whole).
fn bare_signature_of(token: &Value) -> Option<&str> {
    if token.get("type").and_then(Value::as_str) != Some(BARE_SIGNATURE_TYPE) {
        return None;
    }
    token.get("signature").and_then(Value::as_str)
}
