//! The rmcp → factor-q boundary for prompts: turning a fetched
//! `GetPromptResult` into the owned, provider-neutral
//! [`PromptSeed`](crate::prompt::PromptSeed) everything downstream
//! works with.

use std::collections::BTreeMap;

use rmcp::model::{GetPromptResult, PromptMessageContent, PromptMessageRole, ResourceContents};

/// Convert a fetched rmcp `GetPromptResult` into the owned, lossless
/// [`PromptSeed`](crate::prompt::PromptSeed) — the rmcp → factor-q
/// boundary for prompts. Everything downstream is provider-neutral.
///
/// rmcp 1.4–1.7 omit `Audio` from `PromptMessageContent` and reject
/// it on the wire, so audio prompt content never reaches here (the
/// fetch fails first). Our [`PromptContent`](crate::prompt::PromptContent)
/// keeps the spec-canonical `Audio` variant regardless — the
/// upgrade past rmcp 1.7 is issue #341.
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
                PromptMessageRole::User => PromptRole::User,
                PromptMessageRole::Assistant => PromptRole::Assistant,
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
fn prompt_content_from_rmcp(content: &PromptMessageContent) -> crate::prompt::PromptContent {
    use crate::prompt::{EmbeddedResource, PromptContent};
    match content {
        PromptMessageContent::Text { text } => PromptContent::Text {
            text: text.clone(),
            meta: crate::prompt::ContentMeta::default(),
        },
        PromptMessageContent::Image { image } => PromptContent::Image {
            data: image.raw.data.clone(),
            mime_type: image.raw.mime_type.clone(),
            meta: content_meta(image.annotations.as_ref(), image.raw.meta.as_ref()),
        },
        PromptMessageContent::ResourceLink { link } => PromptContent::ResourceLink {
            uri: link.raw.uri.clone(),
            name: link.raw.name.clone(),
            meta: content_meta(link.annotations.as_ref(), link.raw.meta.as_ref()),
        },
        PromptMessageContent::Resource { resource } => {
            let annotations = resource.annotations.as_ref();
            let embedded = match &resource.raw.resource {
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
            };
            PromptContent::EmbeddedResource(embedded)
        }
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
