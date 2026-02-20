use serde_json::Value;

pub(crate) struct ParsedEnvelope {
    pub(crate) source: String,
    pub(crate) message_text: String,
    pub(crate) is_sync: bool,
    pub(crate) source_uuid: String,
    pub(crate) source_name: String,
    pub(crate) attachments: Vec<AttachmentInfo>,
}

/// Parse a Signal envelope JSON into structured fields.
/// Returns None if the envelope should be skipped (no source, no text/attachments, etc.)
pub(crate) fn parse_envelope(envelope: &Value) -> Option<ParsedEnvelope> {
    let source = envelope["envelope"]["sourceNumber"]
        .as_str()
        .or_else(|| envelope["envelope"]["source"].as_str());
    let source = match source {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return None,
    };

    let has_data_attachments = envelope["envelope"]["dataMessage"]["attachments"]
        .as_array()
        .is_some_and(|a| !a.is_empty());
    let has_sync_attachments = envelope["envelope"]["syncMessage"]["sentMessage"]["attachments"]
        .as_array()
        .is_some_and(|a| !a.is_empty());

    let (message_text, is_sync) = if let Some(m) =
        envelope["envelope"]["dataMessage"]["message"].as_str()
    {
        if m.is_empty() && !has_data_attachments {
            return None;
        }
        (m.to_string(), false)
    } else if let Some(m) = envelope["envelope"]["syncMessage"]["sentMessage"]["message"].as_str() {
        if m.is_empty() && !has_sync_attachments {
            return None;
        }
        (m.to_string(), true)
    } else if has_data_attachments {
        ("Describe this attachment.".to_string(), false)
    } else if has_sync_attachments {
        ("Describe this attachment.".to_string(), true)
    } else {
        return None;
    };

    let source_uuid = envelope["envelope"]["sourceUuid"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let source_name = envelope["envelope"]["sourceName"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();

    let attachments = extract_attachments(envelope);

    Some(ParsedEnvelope {
        source,
        message_text,
        is_sync,
        source_uuid,
        source_name,
        attachments,
    })
}

#[derive(Debug, PartialEq)]
pub(crate) enum AttachmentType {
    Image,
    Audio,
    Document,
    Other,
}

pub(crate) fn classify_attachment(content_type: &str) -> AttachmentType {
    if content_type.starts_with("image/") {
        AttachmentType::Image
    } else if content_type.starts_with("audio/") {
        AttachmentType::Audio
    } else if content_type.starts_with("text/") || content_type == "application/pdf" {
        AttachmentType::Document
    } else {
        AttachmentType::Other
    }
}

#[allow(dead_code)] // voice_note used in tests; will be used for voice-vs-audio distinction
pub(crate) struct AttachmentInfo {
    pub(crate) id: String,
    pub(crate) content_type: String,
    pub(crate) filename: Option<String>,
    pub(crate) voice_note: bool,
}

pub(crate) fn extract_attachments(envelope: &Value) -> Vec<AttachmentInfo> {
    let arrays = [
        &envelope["envelope"]["dataMessage"]["attachments"],
        &envelope["envelope"]["syncMessage"]["sentMessage"]["attachments"],
    ];
    for arr in arrays {
        if let Some(list) = arr.as_array() {
            return list
                .iter()
                .filter_map(|a| {
                    let id = a["id"].as_str()?.to_string();
                    let content_type = a["contentType"].as_str()?.to_string();
                    let filename = a["filename"].as_str().map(|s| s.to_string());
                    let voice_note = a["voiceNote"].as_bool().unwrap_or(false);
                    Some(AttachmentInfo {
                        id,
                        content_type,
                        filename,
                        voice_note,
                    })
                })
                .collect();
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_attachment_image() {
        assert!(matches!(
            classify_attachment("image/jpeg"),
            AttachmentType::Image
        ));
        assert!(matches!(
            classify_attachment("image/png"),
            AttachmentType::Image
        ));
        assert!(matches!(
            classify_attachment("image/gif"),
            AttachmentType::Image
        ));
        assert!(matches!(
            classify_attachment("image/webp"),
            AttachmentType::Image
        ));
    }

    #[test]
    fn test_classify_attachment_audio() {
        assert!(matches!(
            classify_attachment("audio/aac"),
            AttachmentType::Audio
        ));
        assert!(matches!(
            classify_attachment("audio/mpeg"),
            AttachmentType::Audio
        ));
        assert!(matches!(
            classify_attachment("audio/ogg"),
            AttachmentType::Audio
        ));
    }

    #[test]
    fn test_classify_attachment_document() {
        assert!(matches!(
            classify_attachment("application/pdf"),
            AttachmentType::Document
        ));
        assert!(matches!(
            classify_attachment("text/plain"),
            AttachmentType::Document
        ));
        assert!(matches!(
            classify_attachment("text/csv"),
            AttachmentType::Document
        ));
    }

    #[test]
    fn test_classify_attachment_other() {
        assert!(matches!(
            classify_attachment("application/octet-stream"),
            AttachmentType::Other
        ));
        assert!(matches!(
            classify_attachment("application/zip"),
            AttachmentType::Other
        ));
        assert!(matches!(
            classify_attachment("video/mp4"),
            AttachmentType::Other
        ));
    }

    #[test]
    fn test_supported_audio_formats() {
        assert!(matches!(
            classify_attachment("audio/aac"),
            AttachmentType::Audio
        ));
        assert!(matches!(
            classify_attachment("audio/ogg"),
            AttachmentType::Audio
        ));
        assert!(matches!(
            classify_attachment("audio/mpeg"),
            AttachmentType::Audio
        ));
        assert!(matches!(
            classify_attachment("audio/x-caf"),
            AttachmentType::Audio
        ));
        assert!(matches!(
            classify_attachment("audio/mp4"),
            AttachmentType::Audio
        ));
    }

    #[test]
    fn test_extract_attachments_from_envelope() {
        let envelope: Value = serde_json::json!({
            "envelope": {
                "dataMessage": {
                    "message": "What's in this image?",
                    "attachments": [{
                        "contentType": "image/jpeg",
                        "filename": "photo.jpg",
                        "id": "abc123",
                        "size": 123456
                    }, {
                        "contentType": "application/pdf",
                        "filename": "doc.pdf",
                        "id": "def456",
                        "size": 789
                    }]
                }
            }
        });
        let attachments = extract_attachments(&envelope);
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0].id, "abc123");
        assert_eq!(attachments[0].content_type, "image/jpeg");
        assert_eq!(attachments[0].filename.as_deref(), Some("photo.jpg"));
        assert_eq!(attachments[1].id, "def456");
        assert_eq!(attachments[1].content_type, "application/pdf");
    }

    #[test]
    fn test_extract_attachments_sync_message() {
        let envelope: Value = serde_json::json!({
            "envelope": {
                "syncMessage": {
                    "sentMessage": {
                        "message": "Check this",
                        "attachments": [{
                            "contentType": "image/png",
                            "id": "sync123",
                            "size": 500
                        }]
                    }
                }
            }
        });
        let attachments = extract_attachments(&envelope);
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].id, "sync123");
    }

    #[test]
    fn test_extract_attachments_none() {
        let envelope: Value = serde_json::json!({
            "envelope": {
                "dataMessage": {
                    "message": "No attachments"
                }
            }
        });
        let attachments = extract_attachments(&envelope);
        assert!(attachments.is_empty());
    }

    #[test]
    fn test_extract_voice_note_flag() {
        let envelope: Value = serde_json::json!({
            "envelope": {
                "dataMessage": {
                    "attachments": [{
                        "contentType": "audio/aac",
                        "id": "voice1",
                        "size": 5000,
                        "voiceNote": true
                    }]
                }
            }
        });
        let attachments = extract_attachments(&envelope);
        assert_eq!(attachments.len(), 1);
        assert!(attachments[0].voice_note);
        assert_eq!(attachments[0].content_type, "audio/aac");
    }

    #[test]
    fn test_parse_envelope_data_message() {
        let env: Value = serde_json::json!({
            "envelope": {
                "sourceNumber": "+1111111111",
                "source": "uuid-1",
                "sourceUuid": "uuid-1",
                "sourceName": "Alice",
                "dataMessage": {
                    "message": "Hello world"
                }
            }
        });
        let parsed = parse_envelope(&env).unwrap();
        assert_eq!(parsed.source, "+1111111111");
        assert_eq!(parsed.message_text, "Hello world");
        assert!(!parsed.is_sync);
        assert_eq!(parsed.source_name, "Alice");
    }

    #[test]
    fn test_parse_envelope_sync_message() {
        let env: Value = serde_json::json!({
            "envelope": {
                "source": "uuid-owner",
                "sourceUuid": "uuid-owner",
                "sourceName": "Owner",
                "syncMessage": {
                    "sentMessage": {
                        "message": "Note to self"
                    }
                }
            }
        });
        let parsed = parse_envelope(&env).unwrap();
        assert_eq!(parsed.source, "uuid-owner");
        assert_eq!(parsed.message_text, "Note to self");
        assert!(parsed.is_sync);
    }

    #[test]
    fn test_parse_envelope_no_text_no_attachments() {
        let env: Value = serde_json::json!({
            "envelope": {
                "sourceNumber": "+1111111111",
                "dataMessage": {}
            }
        });
        assert!(parse_envelope(&env).is_none());
    }

    #[test]
    fn test_parse_envelope_empty_text_no_attachments() {
        let env: Value = serde_json::json!({
            "envelope": {
                "sourceNumber": "+1111111111",
                "dataMessage": {
                    "message": ""
                }
            }
        });
        assert!(parse_envelope(&env).is_none());
    }

    #[test]
    fn test_parse_envelope_no_source() {
        let env: Value = serde_json::json!({
            "envelope": {
                "dataMessage": {
                    "message": "Hello"
                }
            }
        });
        assert!(parse_envelope(&env).is_none());
    }

    #[test]
    fn test_parse_envelope_attachment_only() {
        let env: Value = serde_json::json!({
            "envelope": {
                "sourceNumber": "+1111111111",
                "sourceName": "Alice",
                "dataMessage": {
                    "attachments": [{
                        "contentType": "image/jpeg",
                        "id": "att1",
                        "size": 1000
                    }]
                }
            }
        });
        let parsed = parse_envelope(&env).unwrap();
        assert_eq!(parsed.message_text, "Describe this attachment.");
        assert_eq!(parsed.attachments.len(), 1);
    }

    #[test]
    fn test_parse_envelope_with_attachments() {
        let env: Value = serde_json::json!({
            "envelope": {
                "sourceNumber": "+1111111111",
                "sourceName": "Alice",
                "dataMessage": {
                    "message": "Check this out",
                    "attachments": [{
                        "contentType": "application/pdf",
                        "id": "doc1",
                        "filename": "report.pdf"
                    }]
                }
            }
        });
        let parsed = parse_envelope(&env).unwrap();
        assert_eq!(parsed.message_text, "Check this out");
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].content_type, "application/pdf");
    }
}
