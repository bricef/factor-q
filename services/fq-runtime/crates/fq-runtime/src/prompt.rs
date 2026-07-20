//! Owned, provider-neutral representation of an MCP prompt fetched
//! via `prompts/get` — a reusable **seed** (Step 4 of the
//! MCP-client full-spec plan): a message sequence plus the
//! arguments bound when it was fetched plus which server produced
//! it. The natural downstream consumer is a prompt-seeded subagent
//! (the prompt supplies the opening transcript); Step 4 only
//! produces the value.
//!
//! Two layers, kept deliberately separate:
//!
//! - **Capture** — [`PromptContent`] is a 1:1 mirror of the MCP
//!   2025-11-25 `ContentBlock` union (text, image, audio,
//!   resource_link, embedded resource). Every spec variant has a
//!   home, so a fetched prompt round-trips through serde without
//!   losing anything — including variants factor-q cannot yet act
//!   on. This is what we persist. (Note: serialisation here is our
//!   own private format for reducer state, *not* the MCP wire
//!   shape — rmcp owns wire deserialisation; we convert from its
//!   typed values. See `crate::mcp`, the one place rmcp is allowed;
//!   this module is rmcp-free.)
//! - **Handling** — folding captured content into an agent
//!   transcript ([`Message`]) is *fallible*: supported content
//!   renders, unsupported content returns
//!   [`PromptError::NotImplemented`] rather than being silently
//!   dropped. Capturing an image always succeeds; handling it
//!   fails loudly until we build it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::events::{Message, MessageRole};

/// A fetched MCP prompt, materialised into factor-q-owned types as
/// a reusable seed. Captures the server's response losslessly;
/// handling it is a separate, fallible step (see [`to_transcript`]).
///
/// [`to_transcript`]: PromptSeed::to_transcript
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptSeed {
    /// Provenance: the server that produced this prompt.
    pub server: String,
    /// Provenance: the prompt name requested.
    pub name: String,
    /// The arguments bound when fetching, for provenance / replay.
    pub arguments: BTreeMap<String, String>,
    /// Optional human-facing description the server returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The message sequence, captured losslessly.
    pub messages: Vec<PromptSeedMessage>,
}

impl PromptSeed {
    /// Render the full opening transcript into agent [`Message`]s —
    /// **all-or-nothing**. Fails on the first message whose content
    /// is captured but not yet handled, so a partially-rendered
    /// seed never silently misleads a subagent.
    pub fn to_transcript(&self) -> Result<Vec<Message>, PromptError> {
        self.messages
            .iter()
            .map(PromptSeedMessage::to_message)
            .collect()
    }
}

/// One message in a fetched prompt. MCP prompt roles are only
/// user/assistant (no system/tool), captured verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptSeedMessage {
    pub role: PromptRole,
    pub content: PromptContent,
}

impl PromptSeedMessage {
    /// Convert to a factor-q transcript [`Message`], if the content
    /// is supported. Maps the user/assistant role directly.
    pub fn to_message(&self) -> Result<Message, PromptError> {
        Ok(Message {
            role: self.role.into(),
            content: Some(self.content.to_text()?),
            tool_calls: vec![],
            tool_call_id: None,
        })
    }
}

/// The role of a prompt message sender. The MCP spec admits only
/// these two for prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptRole {
    User,
    Assistant,
}

impl From<PromptRole> for MessageRole {
    fn from(role: PromptRole) -> Self {
        match role {
            PromptRole::User => MessageRole::User,
            PromptRole::Assistant => MessageRole::Assistant,
        }
    }
}

/// Optional MCP content annotations + protocol `_meta`, preserved
/// verbatim (as opaque JSON) so capture stays lossless without
/// re-modelling the large, evolving annotation schema. Empty when
/// the server sent neither.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContentMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl ContentMeta {
    pub fn is_empty(&self) -> bool {
        self.annotations.is_none() && self.meta.is_none()
    }
}

/// Lossless capture of MCP prompt-message content — a 1:1 mirror of
/// the spec's `ContentBlock` union. Every variant the protocol can
/// return has a home so nothing is dropped on the floor.
///
/// Note `Audio` is part of the spec and therefore part of this
/// type, even though rmcp 1.4–1.7 omit it from `PromptMessageContent`
/// and reject it on the wire — so an audio prompt currently fails at
/// fetch rather than reaching this layer. The variant is here so our
/// representation is canonical and ready the day that gap closes
/// (tracked in issue #341).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PromptContent {
    /// Plain text.
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "ContentMeta::is_empty")]
        meta: ContentMeta,
    },
    /// Base64-encoded image with a MIME type.
    Image {
        data: String,
        mime_type: String,
        #[serde(default, skip_serializing_if = "ContentMeta::is_empty")]
        meta: ContentMeta,
    },
    /// Base64-encoded audio with a MIME type.
    Audio {
        data: String,
        mime_type: String,
        #[serde(default, skip_serializing_if = "ContentMeta::is_empty")]
        meta: ContentMeta,
    },
    /// A link to a resource the agent can fetch separately.
    ResourceLink {
        uri: String,
        name: String,
        #[serde(default, skip_serializing_if = "ContentMeta::is_empty")]
        meta: ContentMeta,
    },
    /// A resource embedded inline (text or binary blob).
    EmbeddedResource(EmbeddedResource),
}

impl PromptContent {
    /// The protocol kind name, for diagnostics and error messages.
    pub fn kind(&self) -> &'static str {
        match self {
            PromptContent::Text { .. } => "text",
            PromptContent::Image { .. } => "image",
            PromptContent::Audio { .. } => "audio",
            PromptContent::ResourceLink { .. } => "resource_link",
            PromptContent::EmbeddedResource(_) => "resource",
        }
    }

    /// Render this content into agent-visible text, if supported.
    /// Text (and embedded *text* resources) render; everything else
    /// returns [`PromptError::NotImplemented`] today. The match is
    /// exhaustive, so a newly-added content variant is a compile
    /// error here — the gap can never be skipped silently.
    pub fn to_text(&self) -> Result<String, PromptError> {
        match self {
            PromptContent::Text { text, .. } => Ok(text.clone()),
            PromptContent::EmbeddedResource(EmbeddedResource::Text { text, .. }) => {
                Ok(text.clone())
            }
            PromptContent::Image { .. }
            | PromptContent::Audio { .. }
            | PromptContent::ResourceLink { .. }
            | PromptContent::EmbeddedResource(EmbeddedResource::Blob { .. }) => {
                Err(PromptError::NotImplemented(self.kind()))
            }
        }
    }
}

/// A resource embedded directly in a prompt message — either inline
/// text or a base64 binary blob, addressed by its URI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddedResource {
    Text {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        text: String,
        #[serde(default, skip_serializing_if = "ContentMeta::is_empty")]
        meta: ContentMeta,
    },
    Blob {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        blob: String,
        #[serde(default, skip_serializing_if = "ContentMeta::is_empty")]
        meta: ContentMeta,
    },
}

/// Raised when prompt content is captured and well-formed but
/// factor-q does not yet know how to fold it into an agent
/// transcript. Honest about the gap; fails loudly.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PromptError {
    #[error("prompt content '{0}' is captured but not yet supported by factor-q")]
    NotImplemented(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_with(messages: Vec<PromptSeedMessage>) -> PromptSeed {
        PromptSeed {
            server: "everything".to_string(),
            name: "p".to_string(),
            arguments: BTreeMap::from([("k".to_string(), "v".to_string())]),
            description: Some("d".to_string()),
            messages,
        }
    }

    fn msg(role: PromptRole, content: PromptContent) -> PromptSeedMessage {
        PromptSeedMessage { role, content }
    }

    /// Every spec content variant round-trips through our own serde
    /// format byte-for-byte — the proof that capture is lossless.
    #[test]
    fn capture_round_trips_losslessly_for_all_variants() {
        let seed = seed_with(vec![
            msg(
                PromptRole::User,
                PromptContent::Text {
                    text: "hi".to_string(),
                    meta: ContentMeta::default(),
                },
            ),
            msg(
                PromptRole::Assistant,
                PromptContent::Image {
                    data: "aW1n".to_string(),
                    mime_type: "image/png".to_string(),
                    meta: ContentMeta {
                        annotations: Some(serde_json::json!({"audience": ["user"]})),
                        meta: None,
                    },
                },
            ),
            msg(
                PromptRole::User,
                PromptContent::Audio {
                    data: "YXVk".to_string(),
                    mime_type: "audio/wav".to_string(),
                    meta: ContentMeta::default(),
                },
            ),
            msg(
                PromptRole::User,
                PromptContent::ResourceLink {
                    uri: "file:///x.txt".to_string(),
                    name: "x.txt".to_string(),
                    meta: ContentMeta::default(),
                },
            ),
            msg(
                PromptRole::User,
                PromptContent::EmbeddedResource(EmbeddedResource::Text {
                    uri: "res://1".to_string(),
                    mime_type: Some("text/plain".to_string()),
                    text: "body".to_string(),
                    meta: ContentMeta::default(),
                }),
            ),
            msg(
                PromptRole::User,
                PromptContent::EmbeddedResource(EmbeddedResource::Blob {
                    uri: "res://2".to_string(),
                    mime_type: Some("application/octet-stream".to_string()),
                    blob: "YmxvYg==".to_string(),
                    meta: ContentMeta::default(),
                }),
            ),
        ]);

        let json = serde_json::to_string(&seed).expect("serialise");
        let back: PromptSeed = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(seed, back, "seed must round-trip losslessly");
    }

    #[test]
    fn text_and_embedded_text_render_to_transcript() {
        let seed = seed_with(vec![
            msg(
                PromptRole::User,
                PromptContent::Text {
                    text: "hello".to_string(),
                    meta: ContentMeta::default(),
                },
            ),
            msg(
                PromptRole::User,
                PromptContent::EmbeddedResource(EmbeddedResource::Text {
                    uri: "res://1".to_string(),
                    mime_type: None,
                    text: "body".to_string(),
                    meta: ContentMeta::default(),
                }),
            ),
        ]);

        let transcript = seed.to_transcript().expect("renders");
        assert_eq!(transcript.len(), 2);
        assert!(matches!(transcript[0].role, MessageRole::User));
        assert_eq!(transcript[0].content.as_deref(), Some("hello"));
        assert_eq!(transcript[1].content.as_deref(), Some("body"));
    }

    #[test]
    fn unsupported_variants_fail_loudly_with_their_kind() {
        let cases = [
            (
                PromptContent::Image {
                    data: "x".to_string(),
                    mime_type: "image/png".to_string(),
                    meta: ContentMeta::default(),
                },
                "image",
            ),
            (
                PromptContent::Audio {
                    data: "x".to_string(),
                    mime_type: "audio/wav".to_string(),
                    meta: ContentMeta::default(),
                },
                "audio",
            ),
            (
                PromptContent::ResourceLink {
                    uri: "u".to_string(),
                    name: "n".to_string(),
                    meta: ContentMeta::default(),
                },
                "resource_link",
            ),
            (
                PromptContent::EmbeddedResource(EmbeddedResource::Blob {
                    uri: "u".to_string(),
                    mime_type: None,
                    blob: "x".to_string(),
                    meta: ContentMeta::default(),
                }),
                "resource",
            ),
        ];
        for (content, kind) in cases {
            assert_eq!(
                content.to_text(),
                Err(PromptError::NotImplemented(kind)),
                "{kind} should be NotImplemented"
            );
        }
    }

    #[test]
    fn transcript_is_all_or_nothing_on_unsupported_content() {
        let seed = seed_with(vec![
            msg(
                PromptRole::User,
                PromptContent::Text {
                    text: "ok".to_string(),
                    meta: ContentMeta::default(),
                },
            ),
            msg(
                PromptRole::User,
                PromptContent::Audio {
                    data: "x".to_string(),
                    mime_type: "audio/wav".to_string(),
                    meta: ContentMeta::default(),
                },
            ),
        ]);
        assert!(matches!(
            seed.to_transcript(),
            Err(PromptError::NotImplemented("audio"))
        ));
    }
}
