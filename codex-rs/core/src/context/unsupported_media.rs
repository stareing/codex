use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnsupportedMedia {
    text: &'static str,
    content_kind: &'static str,
}

impl UnsupportedMedia {
    pub(crate) const IMAGE: Self = Self {
        text: "image content omitted because you do not support image input",
        content_kind: "images.unsupported",
    };

    /// An image left out of a compaction request: the summary only needs to know one was there, and images
    /// cost many tokens and can make the request itself too long.
    pub(crate) const IMAGE_FOR_SUMMARY: Self = Self {
        text: "[image]",
        content_kind: "images.omitted_for_compaction",
    };

    /// Audio left out of a compaction request, for the same reason as `IMAGE_FOR_SUMMARY`.
    pub(crate) const AUDIO_FOR_SUMMARY: Self = Self {
        text: "[audio]",
        content_kind: "audio.omitted_for_compaction",
    };

    pub(crate) const AUDIO: Self = Self {
        text: "audio content omitted because you do not support audio input",
        content_kind: "audio.unsupported",
    };
}

impl ContextualUserFragment for UnsupportedMedia {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind(self.content_kind.to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> String {
        self.text.to_string()
    }
}
