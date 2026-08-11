//! Image and PDF attachments, and the per-dialect rendering of them.
//!
//! An [`Attachment`] is raw bytes plus a [`MediaType`] the bytes were *checked*
//! to match, plus the file name the bytes came from (two of the three dialects
//! put it on the wire). Every provider takes the same three things and spells
//! them differently, so the type is dialect-neutral and each renderer lives
//! here next to the others:
//!
//! | dialect                        | image                       | PDF                          |
//! |--------------------------------|-----------------------------|------------------------------|
//! | Anthropic Messages             | [`Attachment::anthropic_block`] `image` block | `document` block |
//! | OpenAI Responses (`codex`)     | [`Attachment::responses_item`] `input_image`  | `input_file`     |
//! | OpenAI chat-completions        | [`Attachment::openai_part`] `image_url`       | `file`           |
//!
//! Nothing in hrdr constructs an [`Attachment`] yet — the UI and the attach
//! path are later slices. What is finished here is the model type, the three
//! renderings, and the refusals ([`check_attachments`]) that keep a request the
//! provider would reject off the wire.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::types::{ChatMessage, Role};

/// Anthropic's per-image cap: **5 MB of base64** — the per-attachment ceiling
/// for an image when the user configured none. The tightest of the three
/// dialects (OpenAI Responses allows 50 MB per file), so it is the value that
/// makes hrdr's local refusal match the remote one, since the same
/// [`Attachment`] may be rendered for any of them.
///
/// Decimal MB rather than MiB: Anthropic's docs say "5MB" without saying which,
/// and the smaller reading is the one that can only ever refuse early rather
/// than let through something the API then rejects.
///
/// The check is on the **encoded** size ([`Attachment::encoded_len`]), which is
/// 4/3 of the raw bytes: an image that fits raw can still be over once encoded.
///
/// **This one is a knob** (`max_attachment_bytes`, `$HRDR_MAX_ATTACHMENT_BYTES`
/// — [`check_attachments`]'s `max_attachment_bytes` argument), because it is the
/// only one of the three that is not the same everywhere: OpenAI allows 50 MB
/// per file and a self-hosted server allows whatever it was built to. The other
/// two below stay constants deliberately — they are protocol limits of the
/// request itself, and a user raising one past what the provider accepts would
/// only trade a clear local refusal for an opaque 413 a round later.
const DEFAULT_MAX_IMAGE_BASE64_BYTES: usize = 5_000_000;

/// Anthropic's per-**request** cap: **32 MB** (published as the PDF limit, and
/// the only whole-request byte limit any of the three dialects states). Applied
/// twice in [`check_attachments`]: to a single PDF, which is otherwise
/// unconstrained — one attachment cannot exceed the budget for the whole request
/// — and to the sum of every attachment.
///
/// It is also the *default* per-attachment ceiling for a PDF, which is why a PDF
/// and an image do not share one: Anthropic caps images at 5 MB but says nothing
/// about a single PDF beyond this request budget, so defaulting both to 5 MB
/// would refuse a 20 MB PDF the provider would have accepted. A configured
/// `max_attachment_bytes` is a ceiling the *user* stated, so it does apply to
/// both — see [`per_attachment_limit`].
const MAX_REQUEST_BASE64_BYTES: usize = 32_000_000;

/// Anthropic's per-request image count for 200k-context models: **100**.
const MAX_IMAGES_PER_REQUEST: usize = 100;

/// The media types hrdr can attach: the four image formats every dialect
/// accepts, plus PDF.
///
/// A closed enum rather than a MIME string, so an unsupported type is
/// unrepresentable instead of being caught somewhere downstream — or not caught
/// at all, and rejected by the provider a round later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaType {
    Jpeg,
    Png,
    Gif,
    Webp,
    Pdf,
}

impl MediaType {
    /// The MIME type, as every dialect spells it on the wire.
    pub fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Pdf => "application/pdf",
        }
    }

    /// Whether this is an image (everything except [`Self::Pdf`]).
    pub fn is_image(self) -> bool {
        !matches!(self, Self::Pdf)
    }

    /// The models.dev input modality that covers this type — `"image"` or
    /// `"pdf"`. The catalog lists the two separately (a model can take images
    /// and not PDFs), which is why the gate asks per attachment.
    pub fn modality(self) -> &'static str {
        if self.is_image() { "image" } else { "pdf" }
    }

    /// The type `bytes` actually are, by leading magic number, or `None` for
    /// anything not in this enum.
    ///
    /// A file extension is a claim; the magic number is the fact. Each pattern
    /// is the format's own signature at offset 0 — deliberately strict there:
    /// a PDF with junk before `%PDF-` is tolerated by some readers, but a
    /// provider is under no obligation to, and "the bytes start with the thing
    /// they claim to be" is the check that cannot be talked into a false pass.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Some(Self::Png);
        }
        if bytes.starts_with(b"\xFF\xD8\xFF") {
            return Some(Self::Jpeg);
        }
        if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            return Some(Self::Gif);
        }
        // RIFF container, with the form type at bytes 8..12. The four bytes in
        // between are the chunk size and carry no signature.
        if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            return Some(Self::Webp);
        }
        if bytes.starts_with(b"%PDF-") {
            return Some(Self::Pdf);
        }
        None
    }
}

impl std::fmt::Display for MediaType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.mime())
    }
}

/// One file attached to a user message.
///
/// The bytes are held as `Arc<[u8]>`, not `Vec<u8>`: [`ChatMessage`] is cloned
/// once per round in the agent loop (the whole history, every round), so a
/// `Vec` would memcpy every attached image on every round for the rest of the
/// session. An `Arc` clone is a refcount bump, and the bytes are immutable
/// after construction — validated once, then only ever read — so there is
/// nothing for shared ownership to make harder.
///
/// Base64 encoding happens at render time ([`Self::base64`]) rather than at
/// construction: the same attachment can be rendered for any of the three
/// dialects, and caching the encoding would keep a second copy resident, 4/3
/// the size of the first, for the whole session.
#[derive(Clone, PartialEq, Eq)]
pub struct Attachment {
    bytes: Arc<[u8]>,
    media_type: MediaType,
    filename: String,
}

/// Prints the shape, never the payload: [`ChatMessage`] derives `Debug` and is
/// logged whole in places, and a derived impl would spill megabytes of image
/// bytes into a log line.
impl std::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attachment")
            .field("filename", &self.filename)
            .field("media_type", &self.media_type)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl Attachment {
    /// Build an attachment, refusing bytes that are not what `media_type`
    /// claims ([`MediaType::sniff`]).
    ///
    /// **Size is not checked here.** How large an attachment may be is a
    /// property of where it is going — the endpoint's limit, and the user's
    /// `max_attachment_bytes` over it — not of the bytes, which are
    /// dialect-neutral and may be legal against one endpoint and not another.
    /// Checking it here would mean either hardcoding one provider's number into
    /// construction, or taking a limit argument every caller is free to widen.
    /// It lives in [`check_attachments`] instead: the gate every request passes
    /// through before anything is sent (`Client::chat_stream`), where the
    /// configured value is in scope and no caller can construct around it.
    pub fn new(
        bytes: impl Into<Arc<[u8]>>,
        media_type: MediaType,
        filename: impl Into<String>,
    ) -> Result<Self, AttachmentError> {
        let bytes: Arc<[u8]> = bytes.into();
        let filename = filename.into();
        let actual = MediaType::sniff(&bytes);
        if actual != Some(media_type) {
            return Err(AttachmentError::TypeMismatch {
                filename,
                declared: media_type,
                actual,
            });
        }
        Ok(Self {
            bytes,
            media_type,
            filename,
        })
    }

    /// The validated media type.
    pub fn media_type(&self) -> MediaType {
        self.media_type
    }

    /// The original file name, as the dialects that send one spell it.
    pub fn filename(&self) -> &str {
        &self.filename
    }

    /// How many bytes this becomes once base64-encoded: 4 bytes per 3-byte
    /// group, the last group padded. Computed rather than encoded — the caps
    /// are checked far more often than the payload is rendered.
    pub fn encoded_len(&self) -> usize {
        self.bytes.len().div_ceil(3).saturating_mul(4)
    }

    /// The payload, base64-encoded (standard alphabet, padded — what all three
    /// dialects expect).
    fn base64(&self) -> String {
        STANDARD.encode(&self.bytes)
    }

    /// `data:<mime>;base64,<payload>` — the form both OpenAI dialects take.
    fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type.mime(), self.base64())
    }

    /// The Anthropic Messages content block: `image` for the image types,
    /// `document` for PDF, each with an inline `base64` source.
    pub fn anthropic_block(&self) -> Value {
        let kind = if self.media_type.is_image() {
            "image"
        } else {
            "document"
        };
        json!({
            "type": kind,
            "source": {
                "type": "base64",
                "media_type": self.media_type.mime(),
                "data": self.base64(),
            },
        })
    }

    /// The OpenAI **Responses** content item: `input_image` carrying a data
    /// URL, or `input_file` carrying the file name alongside it.
    pub fn responses_item(&self) -> Value {
        if self.media_type.is_image() {
            json!({
                "type": "input_image",
                "image_url": self.data_url(),
                "detail": "auto",
            })
        } else {
            json!({
                "type": "input_file",
                "filename": self.filename,
                "file_data": self.data_url(),
            })
        }
    }

    /// The OpenAI **chat-completions** content part: `image_url` or `file`.
    /// This is the dialect that also reaches OpenRouter, DeepSeek and local
    /// servers, so it is the one whose shape has to be the plain documented
    /// one.
    pub fn openai_part(&self) -> Value {
        if self.media_type.is_image() {
            json!({
                "type": "image_url",
                "image_url": { "url": self.data_url(), "detail": "auto" },
            })
        } else {
            json!({
                "type": "file",
                "file": { "filename": self.filename, "file_data": self.data_url() },
            })
        }
    }
}

/// Why an attachment was refused — for the bytes not matching their declared
/// type ([`Attachment::new`]), or by the gate a request passes through before it
/// goes out ([`check_attachments`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentError {
    /// The bytes are not what the declared type says they are.
    TypeMismatch {
        filename: String,
        declared: MediaType,
        /// What the bytes actually are, or `None` for a type hrdr cannot send
        /// at all (including a header too short to identify).
        actual: Option<MediaType>,
    },
    /// One attachment is over the per-attachment cap in force for its type —
    /// the configured `max_attachment_bytes`, or the provider default (see
    /// [`per_attachment_limit`]). `limit` is the number actually applied, so the
    /// message names the user's value when they set one.
    TooLarge {
        filename: String,
        media_type: MediaType,
        encoded: usize,
        limit: usize,
    },
    /// Every attachment in the request, summed, is over the per-request cap.
    RequestTooLarge { encoded: usize, limit: usize },
    /// More images in one request than a request may carry.
    TooManyImages { count: usize, limit: usize },
    /// The model does not take this kind of input.
    Unsupported {
        model: String,
        media_type: MediaType,
    },
    /// An attachment on a message that is not a user turn. No dialect has a
    /// place to put one: Anthropic assistant content is thinking/text/tool_use,
    /// a Responses `function_call_output` is a string, and a chat-completions
    /// tool result is a string too.
    NotUserMessage { role: Role },
}

impl std::fmt::Display for AttachmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TypeMismatch {
                filename,
                declared,
                actual,
            } => match actual {
                Some(actual) => write!(
                    f,
                    "{filename}: declared {declared}, but the bytes are {actual}"
                ),
                None => write!(
                    f,
                    "{filename}: declared {declared}, but the bytes are not a supported image or PDF"
                ),
            },
            Self::TooLarge {
                filename,
                media_type,
                encoded,
                limit,
            } => write!(
                f,
                "{filename}: {encoded} bytes once base64-encoded, over the {limit}-byte \
                 per-attachment limit for {media_type}"
            ),
            Self::RequestTooLarge { encoded, limit } => write!(
                f,
                "attachments total {encoded} bytes once base64-encoded, over the {limit}-byte \
                 per-request limit (Anthropic)"
            ),
            Self::TooManyImages { count, limit } => write!(
                f,
                "{count} images in one request, over the {limit}-image per-request limit \
                 (Anthropic)"
            ),
            Self::Unsupported { model, media_type } => write!(
                f,
                "model {model} does not accept {media_type} input (models.dev lists no \
                 \"{}\" input modality for it)",
                media_type.modality()
            ),
            Self::NotUserMessage { role } => write!(
                f,
                "attachments belong on a user message; found one on a {role:?} message"
            ),
        }
    }
}

impl std::error::Error for AttachmentError {}

/// The per-attachment ceiling in force for `media_type`.
///
/// A configured `max_attachment_bytes` applies to **every** attachment: it is a
/// ceiling the user stated — "nothing over this goes out" — and honouring it for
/// images while letting a PDF past would make the setting mean something other
/// than what it says.
///
/// With nothing configured each type gets the provider's own documented cap
/// ([`DEFAULT_MAX_IMAGE_BASE64_BYTES`] for an image, [`MAX_REQUEST_BASE64_BYTES`]
/// for a PDF, which Anthropic bounds only by the request budget). One shared
/// default of 5 MB would refuse a 20 MB PDF that is perfectly legal — a limit
/// hrdr invented, on a request the provider would have taken.
fn per_attachment_limit(media_type: MediaType, max_attachment_bytes: Option<usize>) -> usize {
    max_attachment_bytes.unwrap_or(if media_type.is_image() {
        DEFAULT_MAX_IMAGE_BASE64_BYTES
    } else {
        MAX_REQUEST_BASE64_BYTES
    })
}

/// Refuse a request whose attachments the provider would reject anyway — and the
/// one place the per-attachment size cap is enforced, so nothing that constructs
/// an [`Attachment`] can get around it.
///
/// `accepts` is the model's models.dev input-modality list
/// ([`crate::catalog::input_modalities_cached`]), or `None` when the catalog
/// has no entry for the model.
///
/// `max_attachment_bytes` is the user's configured per-attachment ceiling
/// (`max_attachment_bytes` in config, `$HRDR_MAX_ATTACHMENT_BYTES`), or `None`
/// for the provider defaults — see [`per_attachment_limit`].
///
/// **The unknown model is allowed through.** hrdr points at llama.cpp, vLLM,
/// Ollama and any other OpenAI-compatible endpoint, and none of their model ids
/// are in models.dev — nor are freshly released ones, until the catalog catches
/// up. Refusing on "not known to support images" would make attachments
/// permanently unusable against every self-hosted server, which is a class of
/// endpoint hrdr exists to support. The cost of the other direction is one
/// wasted round and a provider error, which is recoverable and visible; the
/// cost of refusing is a feature that silently does not exist. (This is the
/// opposite call from [`crate::catalog::is_free_model`], where unknown means
/// *not* free — there the harm runs the other way: an unpriced model offered as
/// free is a row in the picker that cannot work at all.)
pub fn check_attachments(
    model: &str,
    accepts: Option<&[String]>,
    messages: &[ChatMessage],
    max_attachment_bytes: Option<usize>,
) -> Result<(), AttachmentError> {
    let mut total = 0usize;
    let mut images = 0usize;
    for m in messages {
        if m.attachments.is_empty() {
            continue;
        }
        if m.role != Role::User {
            return Err(AttachmentError::NotUserMessage { role: m.role });
        }
        for a in &m.attachments {
            if let Some(accepts) = accepts
                && !accepts.iter().any(|mode| mode == a.media_type.modality())
            {
                return Err(AttachmentError::Unsupported {
                    model: model.to_string(),
                    media_type: a.media_type,
                });
            }
            let encoded = a.encoded_len();
            let limit = per_attachment_limit(a.media_type, max_attachment_bytes);
            if encoded > limit {
                return Err(AttachmentError::TooLarge {
                    filename: a.filename.clone(),
                    media_type: a.media_type,
                    encoded,
                    limit,
                });
            }
            total = total.saturating_add(encoded);
            if a.media_type.is_image() {
                images += 1;
            }
        }
    }
    if images > MAX_IMAGES_PER_REQUEST {
        return Err(AttachmentError::TooManyImages {
            count: images,
            limit: MAX_IMAGES_PER_REQUEST,
        });
    }
    if total > MAX_REQUEST_BASE64_BYTES {
        return Err(AttachmentError::RequestTooLarge {
            encoded: total,
            limit: MAX_REQUEST_BASE64_BYTES,
        });
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal but genuine PNG header followed by `pad` filler bytes.
    pub(crate) fn png(pad: usize) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.resize(v.len() + pad, 0);
        v
    }

    /// A minimal but genuine PDF header followed by `pad` filler bytes.
    pub(crate) fn pdf(pad: usize) -> Vec<u8> {
        let mut v = b"%PDF-1.7".to_vec();
        v.resize(v.len() + pad, 0);
        v
    }

    pub(crate) fn png_attachment(name: &str) -> Attachment {
        Attachment::new(png(4), MediaType::Png, name).expect("valid png")
    }

    pub(crate) fn pdf_attachment(name: &str) -> Attachment {
        Attachment::new(pdf(4), MediaType::Pdf, name).expect("valid pdf")
    }

    /// The modality lists the gate is fed in tests.
    pub(crate) fn modalities(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    /// Each signature is recognized, and only from offset 0.
    #[test]
    fn sniff_recognizes_every_supported_signature() {
        assert_eq!(MediaType::sniff(&png(0)), Some(MediaType::Png));
        assert_eq!(
            MediaType::sniff(b"\xFF\xD8\xFF\xE0junk"),
            Some(MediaType::Jpeg)
        );
        assert_eq!(MediaType::sniff(b"GIF87a...."), Some(MediaType::Gif));
        assert_eq!(MediaType::sniff(b"GIF89a...."), Some(MediaType::Gif));
        assert_eq!(
            MediaType::sniff(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some(MediaType::Webp)
        );
        assert_eq!(MediaType::sniff(&pdf(0)), Some(MediaType::Pdf));
        // Not at offset 0 is not a match.
        assert_eq!(MediaType::sniff(b"\n%PDF-1.7"), None);
        // A RIFF container that is not WebP (a .wav) is not an image.
        assert_eq!(MediaType::sniff(b"RIFF\x24\x08\x00\x00WAVEfmt "), None);
        assert_eq!(MediaType::sniff(b"not anything"), None);
        assert_eq!(MediaType::sniff(b""), None);
    }

    /// Construction validates the bytes against the declared type, and the
    /// error names both sides.
    #[test]
    fn construction_refuses_bytes_that_are_not_what_they_claim() {
        // The honest case.
        let ok = Attachment::new(png(16), MediaType::Png, "shot.png").unwrap();
        assert_eq!(ok.media_type(), MediaType::Png);
        assert_eq!(ok.filename(), "shot.png");

        // A PNG claiming to be a PDF.
        let err = Attachment::new(png(16), MediaType::Pdf, "report.pdf").unwrap_err();
        assert_eq!(
            err,
            AttachmentError::TypeMismatch {
                filename: "report.pdf".to_string(),
                declared: MediaType::Pdf,
                actual: Some(MediaType::Png),
            }
        );
        let msg = err.to_string();
        assert!(
            msg.contains("application/pdf") && msg.contains("image/png"),
            "the error names both types: {msg}"
        );

        // A header truncated below its signature length identifies as nothing.
        let err = Attachment::new(b"\x89PNG\r\n".to_vec(), MediaType::Png, "cut.png").unwrap_err();
        assert_eq!(
            err,
            AttachmentError::TypeMismatch {
                filename: "cut.png".to_string(),
                declared: MediaType::Png,
                actual: None,
            }
        );
        // WebP needs all 12 bytes before it is a WebP.
        assert!(
            Attachment::new(
                b"RIFF\x00\x00\x00\x00WEB".to_vec(),
                MediaType::Webp,
                "a.webp"
            )
            .is_err()
        );
    }

    /// The encoded length is base64's 4-per-3-bytes with padding — the number
    /// every cap is measured in.
    #[test]
    fn encoded_len_matches_the_actual_encoding() {
        for pad in [0, 1, 2, 3, 4, 5, 100] {
            let a = Attachment::new(png(pad), MediaType::Png, "a.png").unwrap();
            assert_eq!(
                a.encoded_len(),
                a.base64().len(),
                "computed length must equal the real encoding (pad={pad})"
            );
        }
    }

    /// One user message carrying `attachments` — what the gate walks.
    pub(crate) fn message_with(attachments: Vec<Attachment>) -> Vec<ChatMessage> {
        let mut m = ChatMessage::user("here");
        m.attachments = attachments;
        vec![m]
    }

    /// Construction validates the bytes and nothing else: an image far over
    /// every cap builds fine, because how big is too big belongs to the endpoint
    /// it is headed for, not to the bytes. The refusal comes from the gate.
    #[test]
    fn construction_does_not_check_the_size() {
        let huge = Attachment::new(png(MAX_REQUEST_BASE64_BYTES), MediaType::Png, "huge.png")
            .expect("size is not a construction-time property");
        assert!(huge.encoded_len() > MAX_REQUEST_BASE64_BYTES);
        assert!(check_attachments("m", None, &message_with(vec![huge]), None).is_err());
    }

    /// With nothing configured, the per-image cap is Anthropic's 5 MB, measured
    /// on the **encoded** size, and it is a boundary: the largest image that fits
    /// passes, one byte more is refused, and the error names the limit.
    #[test]
    fn the_default_per_image_cap_is_exact_at_the_boundary() {
        // 4 encoded bytes per 3 raw: the largest raw size encoding to exactly
        // the cap.
        let at_limit = DEFAULT_MAX_IMAGE_BASE64_BYTES / 4 * 3;
        let a = Attachment::new(png(at_limit - 8), MediaType::Png, "big.png").unwrap();
        assert_eq!(a.encoded_len(), DEFAULT_MAX_IMAGE_BASE64_BYTES);
        assert_eq!(
            check_attachments("m", None, &message_with(vec![a]), None),
            Ok(())
        );

        let over = Attachment::new(png(at_limit - 8 + 1), MediaType::Png, "big.png").unwrap();
        let err = check_attachments("m", None, &message_with(vec![over]), None).unwrap_err();
        let AttachmentError::TooLarge { encoded, limit, .. } = &err else {
            panic!("expected TooLarge, got {err:?}");
        };
        assert_eq!(*limit, DEFAULT_MAX_IMAGE_BASE64_BYTES);
        assert!(*encoded > DEFAULT_MAX_IMAGE_BASE64_BYTES);
        assert!(
            err.to_string()
                .contains(&DEFAULT_MAX_IMAGE_BASE64_BYTES.to_string()),
            "the error names the limit: {err}"
        );

        // A PDF the same size is fine — the 5 MB default is Anthropic's *image*
        // limit; a PDF is bounded by the 32 MB request budget instead. Sharing
        // one default would refuse a document the provider accepts.
        let doc = Attachment::new(pdf(at_limit), MediaType::Pdf, "big.pdf").unwrap();
        assert_eq!(
            check_attachments("m", None, &message_with(vec![doc]), None),
            Ok(())
        );
    }

    /// A single PDF cannot exceed the whole request's byte budget — its default
    /// per-attachment ceiling, since one attachment cannot be bigger than the
    /// request carrying it.
    #[test]
    fn a_single_pdf_cannot_exceed_the_request_budget() {
        let at_limit = MAX_REQUEST_BASE64_BYTES / 4 * 3;
        let ok = Attachment::new(pdf(at_limit - 8), MediaType::Pdf, "ok.pdf").unwrap();
        assert_eq!(
            check_attachments("m", None, &message_with(vec![ok]), None),
            Ok(())
        );

        let over = Attachment::new(pdf(at_limit - 8 + 1), MediaType::Pdf, "over.pdf").unwrap();
        let err = check_attachments("m", None, &message_with(vec![over]), None).unwrap_err();
        let AttachmentError::TooLarge { limit, .. } = &err else {
            panic!("expected TooLarge, got {err:?}");
        };
        assert_eq!(*limit, MAX_REQUEST_BASE64_BYTES);
        assert!(
            err.to_string()
                .contains(&MAX_REQUEST_BASE64_BYTES.to_string()),
            "the error names the limit: {err}"
        );
    }

    /// A configured `max_attachment_bytes` is the ceiling actually applied, and
    /// it applies to images and PDFs alike: the refusal names the user's number,
    /// not the built-in default.
    #[test]
    fn a_configured_cap_is_the_one_enforced_for_every_type() {
        // Small enough that a filler PNG/PDF is comfortably over it.
        let cap = 200usize;
        let fits = Attachment::new(png(4), MediaType::Png, "small.png").unwrap();
        assert!(fits.encoded_len() <= cap);
        assert_eq!(
            check_attachments("m", None, &message_with(vec![fits]), Some(cap)),
            Ok(())
        );

        for (bytes, media_type, name) in [
            (png(400), MediaType::Png, "over.png"),
            (pdf(400), MediaType::Pdf, "over.pdf"),
        ] {
            let a = Attachment::new(bytes, media_type, name).unwrap();
            let encoded = a.encoded_len();
            let err = check_attachments("m", None, &message_with(vec![a]), Some(cap)).unwrap_err();
            assert_eq!(
                err,
                AttachmentError::TooLarge {
                    filename: name.to_string(),
                    media_type,
                    encoded,
                    limit: cap,
                }
            );
            let msg = err.to_string();
            assert!(
                msg.contains("200"),
                "the error names the configured cap: {msg}"
            );
            assert!(
                !msg.contains(&DEFAULT_MAX_IMAGE_BASE64_BYTES.to_string())
                    && !msg.contains(&MAX_REQUEST_BASE64_BYTES.to_string()),
                "the default must not appear once a cap is configured: {msg}"
            );
        }
    }

    /// The knob raises as well as lowers — the point of having it, for an
    /// endpoint whose own limit is higher than Anthropic's (OpenAI allows 50 MB
    /// per file, a self-hosted server whatever it was built for).
    #[test]
    fn a_configured_cap_can_raise_the_image_limit() {
        let raw = DEFAULT_MAX_IMAGE_BASE64_BYTES / 4 * 3 + 3_000;
        let big = Attachment::new(png(raw), MediaType::Png, "big.png").unwrap();
        assert!(big.encoded_len() > DEFAULT_MAX_IMAGE_BASE64_BYTES);
        let msgs = message_with(vec![big]);
        assert!(check_attachments("m", None, &msgs, None).is_err());
        assert_eq!(
            check_attachments("m", None, &msgs, Some(8_000_000)),
            Ok(()),
            "a raised cap lets through what the default refused"
        );
    }

    /// The request-total cap counts every attachment across every message, and
    /// is a boundary too.
    #[test]
    fn the_request_total_cap_is_exact_at_the_boundary() {
        // Two PDFs, each half the budget: exactly at the cap.
        let half_raw = MAX_REQUEST_BASE64_BYTES / 2 / 4 * 3;
        let half = Attachment::new(pdf(half_raw - 8), MediaType::Pdf, "half.pdf").unwrap();
        assert_eq!(half.encoded_len() * 2, MAX_REQUEST_BASE64_BYTES);
        let mut m = ChatMessage::user("here");
        m.attachments = vec![half.clone(), half.clone()];
        assert_eq!(
            check_attachments("m", None, std::slice::from_ref(&m), None),
            Ok(())
        );

        // One more group of three raw bytes pushes it over.
        let over = Attachment::new(pdf(half_raw - 8 + 3), MediaType::Pdf, "half.pdf").unwrap();
        m.attachments = vec![half, over];
        let err = check_attachments("m", None, &[m], None).unwrap_err();
        let AttachmentError::RequestTooLarge { encoded, limit } = &err else {
            panic!("expected RequestTooLarge, got {err:?}");
        };
        assert_eq!(*limit, MAX_REQUEST_BASE64_BYTES);
        assert_eq!(*encoded, MAX_REQUEST_BASE64_BYTES + 4);
        assert!(
            err.to_string()
                .contains(&MAX_REQUEST_BASE64_BYTES.to_string()),
            "the error names the limit: {err}"
        );
    }

    /// The image-count cap is a boundary, and PDFs do not count toward it.
    #[test]
    fn the_image_count_cap_is_exact_at_the_boundary() {
        let img = png_attachment("a.png");
        let mut m = ChatMessage::user("look");
        m.attachments = vec![img.clone(); MAX_IMAGES_PER_REQUEST];
        assert_eq!(
            check_attachments("m", None, std::slice::from_ref(&m), None),
            Ok(())
        );

        // A PDF on top of a full complement of images is still fine.
        m.attachments.push(pdf_attachment("a.pdf"));
        assert_eq!(
            check_attachments("m", None, std::slice::from_ref(&m), None),
            Ok(())
        );

        // One image over, spread across two messages, is not.
        let mut second = ChatMessage::user("and this");
        second.attachments = vec![img];
        let err = check_attachments("m", None, &[m, second], None).unwrap_err();
        assert_eq!(
            err,
            AttachmentError::TooManyImages {
                count: MAX_IMAGES_PER_REQUEST + 1,
                limit: MAX_IMAGES_PER_REQUEST,
            }
        );
        assert!(
            err.to_string()
                .contains(&MAX_IMAGES_PER_REQUEST.to_string()),
            "the error names the limit: {err}"
        );
    }

    /// The capability gate: a model the catalog says takes images takes them, a
    /// model it says is text-only does not, and the two modalities are asked
    /// about separately.
    #[test]
    fn the_capability_gate_reads_the_modality_list() {
        let mut with_image = ChatMessage::user("what is this");
        with_image.attachments = vec![png_attachment("a.png")];
        let mut with_pdf = ChatMessage::user("read this");
        with_pdf.attachments = vec![pdf_attachment("a.pdf")];

        let vision = modalities(&["text", "image", "pdf"]);
        assert_eq!(
            check_attachments(
                "claude-opus-4-6",
                Some(&vision),
                std::slice::from_ref(&with_image),
                None
            ),
            Ok(())
        );
        assert_eq!(
            check_attachments(
                "claude-opus-4-6",
                Some(&vision),
                std::slice::from_ref(&with_pdf),
                None
            ),
            Ok(())
        );

        let text_only = modalities(&["text"]);
        let err = check_attachments(
            "deepseek-v4-pro",
            Some(&text_only),
            std::slice::from_ref(&with_image),
            None,
        )
        .unwrap_err();
        assert_eq!(
            err,
            AttachmentError::Unsupported {
                model: "deepseek-v4-pro".to_string(),
                media_type: MediaType::Png,
            }
        );
        let msg = err.to_string();
        assert!(
            msg.contains("deepseek-v4-pro") && msg.contains("image/png"),
            "the error names the model and what it cannot take: {msg}"
        );

        // Images but no PDFs: the image passes, the PDF does not.
        let images_only = modalities(&["text", "image"]);
        assert_eq!(
            check_attachments(
                "gpt-4-turbo",
                Some(&images_only),
                std::slice::from_ref(&with_image),
                None
            ),
            Ok(())
        );
        assert!(
            check_attachments(
                "gpt-4-turbo",
                Some(&images_only),
                std::slice::from_ref(&with_pdf),
                None
            )
            .is_err()
        );
    }

    /// A model the catalog has never heard of — a local server, an unlisted id
    /// — is allowed through. See [`check_attachments`] for why this direction.
    #[test]
    fn an_unknown_model_is_allowed_to_take_attachments() {
        let mut m = ChatMessage::user("what is this");
        m.attachments = vec![png_attachment("a.png"), pdf_attachment("a.pdf")];
        assert_eq!(
            check_attachments("qwen3-vl-local", None, &[m], None),
            Ok(())
        );
    }

    /// A message with no attachments never trips the gate, whatever the model
    /// accepts.
    #[test]
    fn a_request_without_attachments_is_never_refused() {
        let text_only = modalities(&["text"]);
        let msgs = vec![
            ChatMessage::system("be brief"),
            ChatMessage::user("hi"),
            ChatMessage::assistant("hello"),
            ChatMessage::tool_result("t1", "output"),
        ];
        assert_eq!(
            check_attachments("deepseek-v4-pro", Some(&text_only), &msgs, None),
            Ok(())
        );
        assert_eq!(
            check_attachments("deepseek-v4-pro", None, &msgs, None),
            Ok(())
        );
    }

    /// Attachments on anything but a user turn are refused before the request
    /// goes out — no dialect has a place to render one, so the alternative is
    /// dropping them silently.
    #[test]
    fn attachments_outside_a_user_message_are_refused() {
        for role in [Role::Assistant, Role::System, Role::Tool] {
            let mut m = ChatMessage::user("x");
            m.role = role;
            m.attachments = vec![png_attachment("a.png")];
            assert_eq!(
                check_attachments("m", None, &[m], None),
                Err(AttachmentError::NotUserMessage { role })
            );
        }
    }

    /// The Anthropic block shapes, exactly as the Messages API spells them.
    #[test]
    fn anthropic_blocks_match_the_documented_shape() {
        let img = Attachment::new(png(1), MediaType::Png, "a.png").unwrap();
        assert_eq!(
            img.anthropic_block(),
            json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": "iVBORw0KGgoA",
                },
            })
        );
        let doc = Attachment::new(pdf(1), MediaType::Pdf, "a.pdf").unwrap();
        assert_eq!(
            doc.anthropic_block(),
            json!({
                "type": "document",
                "source": {
                    "type": "base64",
                    "media_type": "application/pdf",
                    "data": "JVBERi0xLjcA",
                },
            })
        );
    }

    /// The Responses item shapes.
    #[test]
    fn responses_items_match_the_documented_shape() {
        let img = Attachment::new(png(1), MediaType::Png, "a.png").unwrap();
        assert_eq!(
            img.responses_item(),
            json!({
                "type": "input_image",
                "image_url": "data:image/png;base64,iVBORw0KGgoA",
                "detail": "auto",
            })
        );
        let doc = Attachment::new(pdf(1), MediaType::Pdf, "doc.pdf").unwrap();
        assert_eq!(
            doc.responses_item(),
            json!({
                "type": "input_file",
                "filename": "doc.pdf",
                "file_data": "data:application/pdf;base64,JVBERi0xLjcA",
            })
        );
    }

    /// The chat-completions part shapes.
    #[test]
    fn openai_parts_match_the_documented_shape() {
        let img = Attachment::new(png(1), MediaType::Png, "a.png").unwrap();
        assert_eq!(
            img.openai_part(),
            json!({
                "type": "image_url",
                "image_url": {
                    "url": "data:image/png;base64,iVBORw0KGgoA",
                    "detail": "auto",
                },
            })
        );
        let doc = Attachment::new(pdf(1), MediaType::Pdf, "doc.pdf").unwrap();
        assert_eq!(
            doc.openai_part(),
            json!({
                "type": "file",
                "file": {
                    "filename": "doc.pdf",
                    "file_data": "data:application/pdf;base64,JVBERi0xLjcA",
                },
            })
        );
    }

    /// `Debug` prints the byte *count*, never the bytes — a `ChatMessage` with
    /// an image in it is logged whole in places.
    #[test]
    fn debug_never_prints_the_payload() {
        let a = Attachment::new(png(2048), MediaType::Png, "a.png").unwrap();
        let s = format!("{a:?}");
        assert!(s.contains("a.png") && s.contains("2056"), "{s}");
        assert!(s.len() < 200, "debug output stays small: {s}");
    }
}
