//! The prompt boundary converts what a server actually sends. Each case
//! starts from the wire JSON of a `prompts/get` result, so the tests
//! pin the protocol shape rather than rmcp's constructors.

use std::collections::BTreeMap;

use rmcp::model::GetPromptResult;
use serde_json::json;

use super::prompt_seed_from_rmcp;
use crate::prompt::{ContentMeta, EmbeddedResource, PromptContent, PromptRole};

fn seed_from(messages: serde_json::Value) -> crate::prompt::PromptSeed {
    let result: GetPromptResult = serde_json::from_value(json!({ "messages": messages }))
        .expect("a valid prompts/get result");
    prompt_seed_from_rmcp("srv", "p", BTreeMap::new(), result)
}

#[test]
fn audio_prompt_content_arrives_as_audio() {
    // rmcp 1.4–1.7 rejected this on the wire (issue #341).
    let seed = seed_from(json!([{
        "role": "user",
        "content": { "type": "audio", "data": "AAAA", "mimeType": "audio/wav" }
    }]));
    assert_eq!(seed.messages[0].role, PromptRole::User);
    assert_eq!(
        seed.messages[0].content,
        PromptContent::Audio {
            data: "AAAA".into(),
            mime_type: "audio/wav".into(),
            meta: ContentMeta::default(),
        }
    );
}

#[test]
fn text_keeps_its_annotations() {
    let seed = seed_from(json!([{
        "role": "assistant",
        "content": { "type": "text", "text": "hi", "annotations": { "priority": 0.5 } }
    }]));
    assert_eq!(seed.messages[0].role, PromptRole::Assistant);
    let PromptContent::Text { text, meta } = &seed.messages[0].content else {
        panic!("expected text, got {:?}", seed.messages[0].content);
    };
    assert_eq!(text, "hi");
    assert_eq!(meta.annotations, Some(json!({ "priority": 0.5 })));
    assert_eq!(meta.meta, None);
}

#[test]
fn image_resource_link_and_embedded_resources_convert() {
    let seed = seed_from(json!([
        { "role": "user", "content": { "type": "image", "data": "iVBO", "mimeType": "image/png" } },
        { "role": "user", "content": { "type": "resource_link", "uri": "file:///a.md", "name": "a" } },
        { "role": "user", "content": { "type": "resource", "resource": {
            "uri": "file:///b.md", "mimeType": "text/markdown", "text": "# b", "_meta": { "k": 1 }
        } } },
        { "role": "user", "content": { "type": "resource", "resource": {
            "uri": "file:///c.bin", "blob": "AQID"
        } } }
    ]));
    let contents: Vec<_> = seed.messages.into_iter().map(|m| m.content).collect();
    assert_eq!(
        contents[0],
        PromptContent::Image {
            data: "iVBO".into(),
            mime_type: "image/png".into(),
            meta: ContentMeta::default(),
        }
    );
    assert_eq!(
        contents[1],
        PromptContent::ResourceLink {
            uri: "file:///a.md".into(),
            name: "a".into(),
            meta: ContentMeta::default(),
        }
    );
    assert_eq!(
        contents[2],
        PromptContent::EmbeddedResource(EmbeddedResource::Text {
            uri: "file:///b.md".into(),
            mime_type: Some("text/markdown".into()),
            text: "# b".into(),
            meta: ContentMeta {
                annotations: None,
                meta: Some(json!({ "k": 1 })),
            },
        })
    );
    assert_eq!(
        contents[3],
        PromptContent::EmbeddedResource(EmbeddedResource::Blob {
            uri: "file:///c.bin".into(),
            mime_type: None,
            blob: "AQID".into(),
            meta: ContentMeta::default(),
        })
    );
}
