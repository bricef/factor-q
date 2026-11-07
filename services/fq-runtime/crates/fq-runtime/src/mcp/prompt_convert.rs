//! The rmcp → factor-q boundary for prompts: turning a fetched
//! `GetPromptResult` into the owned, provider-neutral
//! [`PromptSeed`](crate::prompt::PromptSeed) everything downstream
//! works with.

use std::collections::BTreeMap;

use rmcp::model::{ContentBlock, GetPromptResult, ResourceContents, Role};

/// Convert a fetched rmcp `GetPromptResult` into the owned, lossless
/// [`PromptSeed`](crate::prompt::PromptSeed) — the rmcp → factor-q
/// boundary for prompts. Everything downstream is provider-neutral.
///
/// Since rmcp 3, prompt messages carry the same `ContentBlock` as tool
/// results, so audio prompt content arrives and maps to the
/// spec-canonical [`PromptContent::Audio`](crate::prompt::PromptContent)
/// (rmcp 1.4–1.7 rejected it on the wire; issue #341).
pub(super) fn prompt_seed_from_rmcp(
    server: &str,
    name: &str,
    arguments: BTreeMap<String, String>,
    result: GetPromptResult,
) -> crate::prompt::PromptSeed {
    use crate::prompt::{PromptRole, PromptSeed, PromptSeedMessage};
    let messages = result
        .messages
        .iter()
        .map(|m| PromptSeedMessage {
            role: match m.role {
                Role::User => PromptRole::User,
                Role::Assistant => PromptRole::Assistant,
            },
            content: prompt_content_from_rmcp(&m.content),
        })
        .collect();
    PromptSeed {
        server: server.to_string(),
        name: name.to_string(),
        arguments,
        description: result.description.clone(),
        messages,
    }
}

/// Map one rmcp prompt content block to the owned [`PromptContent`].
/// Captures the primary fields plus annotations / `_meta` (verbatim,
/// as opaque JSON) so the conversion is lossless for everything rmcp
/// can deliver.
fn prompt_content_from_rmcp(content: &ContentBlock) -> crate::prompt::PromptContent {
    use crate::prompt::{EmbeddedResource, PromptContent};
    match content {
        ContentBlock::Text(text) => PromptContent::Text {
            text: text.text.clone(),
            meta: content_meta(text.annotations.as_ref(), text.meta.as_ref()),
        },
        ContentBlock::Image(image) => PromptContent::Image {
            data: image.data.clone(),
            mime_type: image.mime_type.clone(),
            meta: content_meta(image.annotations.as_ref(), image.meta.as_ref()),
        },
        ContentBlock::Audio(audio) => PromptContent::Audio {
            data: audio.data.clone(),
            mime_type: audio.mime_type.clone(),
            meta: content_meta(audio.annotations.as_ref(), audio.meta.as_ref()),
        },
        ContentBlock::ResourceLink(link) => PromptContent::ResourceLink {
            uri: link.uri.clone(),
            name: link.name.clone(),
            meta: content_meta(link.annotations.as_ref(), link.meta.as_ref()),
        },
        ContentBlock::Resource(resource) => {
            let annotations = resource.annotations.as_ref();
            let embedded = match &resource.resource {
                ResourceContents::TextResourceContents {
                    uri,
                    mime_type,
                    text,
                    meta,
                } => EmbeddedResource::Text {
                    uri: uri.clone(),
                    mime_type: mime_type.clone(),
                    text: text.clone(),
                    meta: content_meta(annotations, meta.as_ref()),
                },
                ResourceContents::BlobResourceContents {
                    uri,
                    mime_type,
                    blob,
                    meta,
                } => EmbeddedResource::Blob {
                    uri: uri.clone(),
                    mime_type: mime_type.clone(),
                    blob: blob.clone(),
                    meta: content_meta(annotations, meta.as_ref()),
                },
                // Non-exhaustive in rmcp: an embedded-contents kind added
                // later is named, not silently dropped.
                _ => return unsupported_content(),
            };
            PromptContent::EmbeddedResource(embedded)
        }
        // Non-exhaustive in rmcp: a content block kind added later.
        _ => unsupported_content(),
    }
}

/// The placeholder for a content kind this conversion does not know:
/// visible to the model as text rather than dropped from the prompt.
fn unsupported_content() -> crate::prompt::PromptContent {
    crate::prompt::PromptContent::Text {
        text: "[unsupported prompt content]".to_string(),
        meta: crate::prompt::ContentMeta::default(),
    }
}

/// Capture optional rmcp annotations + `_meta` as opaque JSON,
/// keeping the owned prompt types lossless without re-modelling the
/// (large, evolving) annotation schema. Generic so the rmcp
/// `Annotations` / `Meta` types stay out of the owned `prompt` module.
fn content_meta<A: serde::Serialize, M: serde::Serialize>(
    annotations: Option<&A>,
    meta: Option<&M>,
) -> crate::prompt::ContentMeta {
    crate::prompt::ContentMeta {
        annotations: annotations.and_then(|a| serde_json::to_value(a).ok()),
        meta: meta.and_then(|m| serde_json::to_value(m).ok()),
    }
}

#[cfg(test)]
mod tests;
