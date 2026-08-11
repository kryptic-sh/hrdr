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
//! An attachment reaches a message from an `@file` mention, from a `Ctrl+]`
//! paste (image bytes, or a `text/uri-list` naming a file), or from a resumed
//! session's blob store — all of them through [`Attachment::new`], which is
//! also where the bytes are sniffed and where each attachment's token cost is
//! computed once ([`Attachment::estimated_tokens`], the figure the agent's
//! context accounting adds to its per-message estimate).
//!
//! The refusals that keep a request the provider would reject off the wire live
//! here too ([`check_attachments`]): the model's input modalities, the
//! per-attachment size ceiling, the per-request byte and image budgets.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::types::{ChatMessage, Role};

/// Anthropic's per-image cap: **10 MB of base64** — the per-attachment ceiling
/// for an image when the user configured none. The tightest of the three
/// dialects (OpenAI allows 50 MB per file), so it is the value that makes
/// hrdr's local refusal match the remote one, since the same [`Attachment`] may
/// be rendered for any of them.
///
/// The vision docs give three numbers under "The maximum size per image is":
/// "10 MB (base64-encoded) when using the Claude API directly", "5 MB
/// (base64-encoded) on Amazon Bedrock and Google Cloud", and "10 MB on
/// claude.ai". The direct-API number is the one that applies: hrdr speaks the
/// native Messages API only to `*.anthropic.com`
/// (`crate::client::detect_backend`), never to a Bedrock or Vertex endpoint.
///
/// Decimal MB rather than MiB: the docs say "10 MB" without saying which, and
/// the smaller reading is the one that can only ever refuse early rather than
/// let through something the API then rejects.
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
const DEFAULT_MAX_IMAGE_BASE64_BYTES: usize = 10_000_000;

/// Anthropic's per-**request** cap: **32 MB** (published as the PDF limit, and
/// the only whole-request byte limit any of the three dialects states). Applied
/// twice in [`check_attachments`]: to a single PDF, which is otherwise
/// unconstrained — one attachment cannot exceed the budget for the whole request
/// — and to the sum of every attachment.
///
/// It is also the *default* per-attachment ceiling for a PDF, which is why a PDF
/// and an image do not share one: Anthropic caps one image
/// ([`DEFAULT_MAX_IMAGE_BASE64_BYTES`]) but says nothing about a single PDF
/// beyond this request budget, so defaulting both to the image cap would refuse
/// a 20 MB PDF the provider would have accepted. A configured
/// `max_attachment_bytes` is a ceiling the *user* stated, so it does apply to
/// both — see [`per_attachment_limit`].
const MAX_REQUEST_BASE64_BYTES: usize = 32_000_000;

/// Anthropic's per-request image count for 200k-context models: **100**.
///
/// The docs give two figures — "100 per request on the API, for models with a
/// 200k-token context window" and "600 per request on the API, for all other
/// models" — and this is the smaller one, deliberately: the gate is handed a
/// model name, not a context window, so the figure that cannot be wrong in the
/// direction of a 413 is the one it applies.
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

    /// The type a MIME string names, or `None` for one this enum does not
    /// cover. The exact inverse of [`Self::mime`], and the read side of session
    /// persistence: a stored attachment records its type as the MIME string,
    /// and that string has to come back as the same closed enum variant it went
    /// out as.
    pub fn from_mime(mime: &str) -> Option<Self> {
        match mime {
            "image/jpeg" => Some(Self::Jpeg),
            "image/png" => Some(Self::Png),
            "image/gif" => Some(Self::Gif),
            "image/webp" => Some(Self::Webp),
            "application/pdf" => Some(Self::Pdf),
            _ => None,
        }
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

/// Anthropic's visual-token patch: an image costs `⌈width / 28⌉ × ⌈height / 28⌉`
/// tokens, one per 28×28 pixel patch.
const IMAGE_PATCH_PX: u64 = 28;

/// The standard tier's long-edge cap in pixels: an image longer than this on its
/// long edge is downscaled (aspect preserved) before it is charged.
///
/// Nothing is charged at this tier (see [`CHARGED_MAX_EDGE_PX`]); it is kept so
/// the tests can check [`visual_tokens`] against the standard-tier figures
/// Anthropic publishes, which are half the evidence that the formula is
/// transcribed correctly.
#[cfg(test)]
const STANDARD_MAX_EDGE_PX: u64 = 1_568;

/// The standard tier's ceiling on one image's visual tokens. An image still over
/// it after the long-edge downscale is downscaled further until it fits.
/// Test-only, for the same reason as [`STANDARD_MAX_EDGE_PX`].
#[cfg(test)]
const STANDARD_MAX_IMAGE_TOKENS: u64 = 1_568;

/// The high-resolution tier's long-edge cap (Claude 4.7 and later).
const HIGH_RES_MAX_EDGE_PX: u64 = 2_576;

/// The high-resolution tier's ceiling on one image's visual tokens.
const HIGH_RES_MAX_IMAGE_TOKENS: u64 = 4_784;

/// The tier every attachment is charged at, and the reason it is the expensive
/// one: an image is charged the same way for every dialect, so the choice is
/// which way to be wrong. The high-resolution tier is automatic on Claude 4.7
/// and later — the models hrdr points at by default — where the standard tier
/// under-charges a 1080p screenshot by 1,131 tokens, and **under-counting is
/// the error that lets a request overflow the window** while over-counting only
/// compacts a little early. On a standard-tier or non-Anthropic model this runs
/// high (never more than the ratio between the two ceilings), which is the
/// direction a budget estimate should err.
const CHARGED_MAX_EDGE_PX: u64 = HIGH_RES_MAX_EDGE_PX;
const CHARGED_MAX_IMAGE_TOKENS: u64 = HIGH_RES_MAX_IMAGE_TOKENS;

/// What one image is charged when its header cannot be parsed — the most the
/// charged tier can charge for any single image, so an unreadable header can
/// only ever over-count. Zero (what an unaccounted attachment used to cost) is
/// the one answer that lets a request overflow the window.
const UNKNOWN_IMAGE_TOKENS: u32 = CHARGED_MAX_IMAGE_TOKENS as u32;

/// Tokens charged per PDF page. Anthropic converts every page to an image *and*
/// extracts its text, and documents the result as 1,500–3,000 tokens per page;
/// this is the top of that range, because a per-page estimate that runs low is
/// the one that lets a request overflow the window.
const PDF_TOKENS_PER_PAGE: u32 = 3_000;

/// Bytes per page assumed when neither [`crate::pdf::page_count`] nor
/// [`pdf_page_count`] can produce one — a middling figure for a compressed PDF,
/// between a text-only page of a few KB and a scanned page of a few hundred.
const PDF_BYTES_PER_PAGE: usize = 50_000;

/// The pixel dimensions in an image's header, or `None` for a header that is
/// truncated, malformed, or a sub-format this cannot read.
///
/// Every read goes through `get`, and every arithmetic step is checked: these
/// bytes came off a clipboard or a path the user named, so a hostile file must
/// return `None` rather than panic.
fn image_dimensions(media_type: MediaType, bytes: &[u8]) -> Option<(u32, u32)> {
    match media_type {
        MediaType::Png => png_dimensions(bytes),
        MediaType::Jpeg => jpeg_dimensions(bytes),
        MediaType::Gif => gif_dimensions(bytes),
        MediaType::Webp => webp_dimensions(bytes),
        // A PDF has no single set of dimensions — its cost is per page, see
        // [`pdf_tokens`].
        MediaType::Pdf => None,
    }
}

/// Big-endian `u32` at `at`, or `None` if the slice is short.
fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Big-endian `u16` at `at`, or `None` if the slice is short.
fn be_u16(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?) as u32)
}

/// Little-endian `u16` at `at`, or `None` if the slice is short.
fn le_u16(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?) as u32)
}

/// A dimension pair, rejecting a zero in either axis — a header claiming a
/// zero-pixel edge is malformed, and it would divide by zero downstream.
fn dimensions(width: u32, height: u32) -> Option<(u32, u32)> {
    (width > 0 && height > 0).then_some((width, height))
}

/// PNG: the 8-byte signature is followed by the `IHDR` chunk, which the spec
/// requires to come first — 4-byte length, the type, then width and height as
/// big-endian `u32`s at bytes 16 and 20.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    dimensions(be_u32(bytes, 16)?, be_u32(bytes, 20)?)
}

/// JPEG: walk the segment markers to the frame header.
///
/// Dimensions live in a `SOF` marker (`0xC0`–`0xCF`), of which `0xC4` (Huffman
/// tables), `0xC8` (a reserved JPEG extension) and `0xCC` (arithmetic-coding
/// conditioning) are *not* frame headers despite sitting in the range. Its
/// payload is one precision byte, then height and width as big-endian `u16`s —
/// height first, which is the transcription slip this costs nothing to get
/// wrong and everything to notice.
///
/// A marker is `0xFF` plus a code, optionally preceded by `0xFF` fill bytes.
/// Most carry a big-endian length covering itself, which is how the walk skips a
/// segment without understanding it. Start of scan (`0xDA`) is where entropy
/// data begins and the marker structure ends, so a file whose `SOF` has not
/// arrived by then has none to find.
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    // Past the SOI (`FFD8`) that `MediaType::sniff` already matched.
    let mut i = 2usize;
    loop {
        if *bytes.get(i)? != 0xFF {
            return None;
        }
        while *bytes.get(i)? == 0xFF {
            i += 1;
        }
        let marker = *bytes.get(i)?;
        i += 1;
        match marker {
            // Standalone: no length, no payload.
            0x01 | 0xD0..=0xD7 | 0xD8 => continue,
            // End of image, or the start of entropy-coded data: no frame header
            // is coming.
            0x00 | 0xD9 | 0xDA => return None,
            _ => {}
        }
        let len = be_u16(bytes, i)? as usize;
        // The length counts its own two bytes, so anything below 2 is malformed
        // and would leave the walk standing still.
        if len < 2 {
            return None;
        }
        let payload = bytes.get(i + 2..i.checked_add(len)?)?;
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            return dimensions(be_u16(payload, 3)?, be_u16(payload, 1)?);
        }
        i += len;
    }
}

/// GIF: the logical screen descriptor follows the 6-byte signature, opening with
/// width and height as little-endian `u16`s.
fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    dimensions(le_u16(bytes, 6)?, le_u16(bytes, 8)?)
}

/// WebP: a RIFF container whose first chunk names the sub-format, each of which
/// carries the dimensions somewhere else.
///
/// - `VP8 ` (lossy): a key frame's 3-byte tag, the `9D 01 2A` start code, then
///   14-bit width and height each packed under a 2-bit scale field.
/// - `VP8L` (lossless): a `0x2F` signature, then width−1 and height−1 as 14-bit
///   fields of one little-endian `u32`.
/// - `VP8X` (extended): 24-bit little-endian canvas width−1 and height−1.
///
/// An animated or alpha-carrying file is `VP8X`, whose canvas is the size the
/// provider would rasterize, so all three sub-formats are readable. Anything
/// else in that slot is a WebP variant this does not know, and returns `None`
/// rather than a guess.
fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let payload = bytes.get(20..)?;
    match bytes.get(12..16)? {
        b"VP8 " => {
            if payload.get(3..6)? != [0x9D, 0x01, 0x2A] {
                return None;
            }
            dimensions(le_u16(payload, 6)? & 0x3FFF, le_u16(payload, 8)? & 0x3FFF)
        }
        b"VP8L" => {
            if *payload.first()? != 0x2F {
                return None;
            }
            let bits = u32::from_le_bytes(payload.get(1..5)?.try_into().ok()?);
            dimensions((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1)
        }
        b"VP8X" => {
            let axis = |at: usize| -> Option<u32> {
                let b = payload.get(at..at + 3)?;
                Some(u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16)
            };
            dimensions(axis(4)? + 1, axis(7)? + 1)
        }
        _ => None,
    }
}

/// What one image of `width × height` costs, on Anthropic's published rule:
/// `⌈width / 28⌉ × ⌈height / 28⌉` visual tokens, after any downscale needed to
/// bring it inside `max_edge_px` on its long edge and `max_tokens` in total.
///
/// The downscale preserves the aspect ratio and takes the largest size that
/// fits, which is why the token cap is walked one patch column at a time rather
/// than solved in closed form: the two ceilings make the cost a step function,
/// and the largest fitting size is the one just under a step. (1920×1080 is the
/// worked example — the long-edge scale alone lands on 1792 tokens, over the
/// 1568 cap, and the largest aspect-preserving size under it is 1456×819, or
/// 1560 tokens, which is what Anthropic publishes.)
///
/// `max_edge_px`/`max_tokens` are the tier's two numbers. Only the standard tier
/// is charged for ([`Attachment::estimated_tokens`]); the high-resolution tier's
/// pair is passed by the tests, which assert both sets of published reference
/// points and so are what proves the formula is transcribed correctly.
///
/// An aspect ratio extreme enough that even a single patch column exceeds
/// `max_tokens` returns what that column costs — over the tier cap, which is the
/// direction an estimate should err.
fn visual_tokens(width: u32, height: u32, max_edge_px: u64, max_tokens: u64) -> u32 {
    let (long, short) = if width >= height {
        (u64::from(width), u64::from(height))
    } else {
        (u64::from(height), u64::from(width))
    };
    if long == 0 || short == 0 {
        return 0;
    }
    let mut long_px = long.min(max_edge_px);
    loop {
        let cols = long_px.div_ceil(IMAGE_PATCH_PX);
        // The short edge at this scale, in patches: ⌈short × long_px / long / 28⌉
        // in one integer step, so the ratio is never rounded twice.
        let rows = (short * long_px).div_ceil(long * IMAGE_PATCH_PX);
        let tokens = cols * rows;
        if tokens <= max_tokens || cols <= 1 {
            return tokens.min(u64::from(u32::MAX)) as u32;
        }
        // The largest width one patch column narrower — the next size down that
        // can cost less.
        long_px = (cols - 1) * IMAGE_PATCH_PX;
    }
}

/// `/Type /Page` objects in the raw bytes, with the `/Type /Pages` tree nodes
/// excluded — the **fallback** for a file [`crate::pdf::page_count`] could not
/// read, not the primary count.
///
/// It is a scan, so it is wrong in both directions, which is why it is second
/// in line: zero for a PDF whose objects live in compressed object streams
/// (most of them, this decade), and too many for one where the text appears in
/// a content stream, a string or a comment. It stays because it costs one pass
/// over the bytes and is right for the flat, uncompressed files that a real
/// parse also gets right — so when it fires at all, it beats dividing by a
/// constant.
fn pdf_page_count(bytes: &[u8]) -> usize {
    const TYPE: &[u8] = b"/Type";
    let mut count = 0usize;
    let mut rest = bytes;
    while let Some(hit) = rest.windows(TYPE.len()).position(|w| w == TYPE) {
        rest = &rest[hit + TYPE.len()..];
        let skipped = rest
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .unwrap_or(rest.len());
        let name = &rest[skipped..];
        // A name ends at a delimiter, so a trailing alphanumeric means this is
        // `/Pages` (the tree node) or some longer name, not a page.
        if name.starts_with(b"/Page") && !name.get(5).is_some_and(u8::is_ascii_alphanumeric) {
            count += 1;
        }
    }
    count
}

/// A token cost for a PDF: its page count times [`PDF_TOKENS_PER_PAGE`].
///
/// The page count comes from three places, in falling order of how much they
/// know. [`crate::pdf::page_count`] parses the file's cross-reference chain and
/// reads the `/Count` the page tree declares — exact, for every file it can
/// read at all. Failing that, [`pdf_page_count`]'s byte scan. Failing that, the
/// file's size over [`PDF_BYTES_PER_PAGE`], which is a guess and is documented
/// as one.
///
/// Both the per-page rate and the byte fallback are chosen to err high: a
/// document charged more than it costs makes the agent compact early, while one
/// charged less lets a request overflow the window.
fn pdf_tokens(bytes: &[u8]) -> u32 {
    let pages = match crate::pdf::page_count(bytes) {
        Some(declared) => declared as usize,
        None => match pdf_page_count(bytes) {
            0 => bytes.len().div_ceil(PDF_BYTES_PER_PAGE).max(1),
            scanned => scanned,
        },
    };
    u32::try_from(pages)
        .unwrap_or(u32::MAX)
        .saturating_mul(PDF_TOKENS_PER_PAGE)
}

/// What `bytes` will cost in the prompt, as [`Attachment::new`] records it once.
fn estimate_tokens(media_type: MediaType, bytes: &[u8]) -> u32 {
    if media_type == MediaType::Pdf {
        return pdf_tokens(bytes);
    }
    match image_dimensions(media_type, bytes) {
        Some((w, h)) => visual_tokens(w, h, CHARGED_MAX_EDGE_PX, CHARGED_MAX_IMAGE_TOKENS),
        None => UNKNOWN_IMAGE_TOKENS,
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
    /// [`Self::estimated_tokens`], computed once at construction — see there for
    /// why it is stored rather than recomputed.
    estimated_tokens: u32,
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
            .field("tokens", &self.estimated_tokens)
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
        let estimated_tokens = estimate_tokens(media_type, &bytes);
        Ok(Self {
            bytes,
            media_type,
            filename,
            estimated_tokens,
        })
    }

    /// The validated media type.
    pub fn media_type(&self) -> MediaType {
        self.media_type
    }

    /// What this attachment adds to a request's prompt, in tokens — the figure
    /// the agent's context accounting adds per message
    /// (`hrdr_agent::estimate_tokens_in_messages`), so an image-heavy session
    /// compacts on time and its context gauge is not describing the text alone.
    ///
    /// An estimate, deliberately Anthropic's: an image costs
    /// `⌈width / 28⌉ × ⌈height / 28⌉` visual tokens there (see
    /// [`visual_tokens`]), while OpenAI charges 32×32 patches against its own
    /// per-model budgets, so one formula is an approximation for somebody
    /// whichever is picked — and this is a budget estimate, not a bill. The
    /// server's own usage numbers replace it entirely whenever the endpoint
    /// reports any (`hrdr_agent::Agent::account_usage`); this is the fallback
    /// for the endpoints that report none.
    ///
    /// **Computed once, at construction.** It is a pure function of bytes that
    /// are immutable for the attachment's life (`Arc<[u8]>`, validated then only
    /// ever read, and every construction path goes through [`Self::new`]), so
    /// there is nothing for a stored copy to fall out of date with. The
    /// accounting it feeds runs over the whole history on every round, and
    /// re-walking a JPEG's segment markers or rescanning a 20 MB PDF each time
    /// would put a file parse on that path once per attachment per round.
    pub fn estimated_tokens(&self) -> u32 {
        self.estimated_tokens
    }

    /// The original file name, as the dialects that send one spell it.
    pub fn filename(&self) -> &str {
        &self.filename
    }

    /// The raw payload — the bytes that were validated against
    /// [`Self::media_type`] at construction.
    ///
    /// For persistence: a session stores these beside its file, content-addressed
    /// by their digest, rather than inlining them (see `hrdr_agent::session`).
    /// Hashing happens there, not here — this crate has no digest dependency, and
    /// an attachment is a wire concern.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
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
/// default at the image cap would refuse a 20 MB PDF that is perfectly legal —
/// a limit hrdr invented, on a request the provider would have taken.
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

    /// A PNG whose `IHDR` declares `width × height` — the real chunk layout,
    /// with the trailing fields the parser must not need.
    pub(crate) fn png_sized(width: u32, height: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&width.to_be_bytes());
        v.extend_from_slice(&height.to_be_bytes());
        // Bit depth, colour type, compression, filter, interlace, then the CRC.
        v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        v
    }

    /// A JPEG whose `SOF0` declares `width × height`, reached only by walking
    /// past an `APP0` segment, a `DHT` (in the `SOF` marker range but not a
    /// frame header), and a pair of fill bytes.
    fn jpeg_sized(width: u16, height: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        v.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]);
        v.extend_from_slice(b"JFIF\0\x01\x01\x00\x00\x01\x00\x01\x00\x00");
        v.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x04, 0x00, 0x00]);
        v.extend_from_slice(&[0xFF, 0xFF]);
        v.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        v.extend_from_slice(&height.to_be_bytes());
        v.extend_from_slice(&width.to_be_bytes());
        v.extend_from_slice(&[0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01]);
        v
    }

    /// A GIF whose logical screen descriptor declares `width × height`.
    fn gif_sized(width: u16, height: u16) -> Vec<u8> {
        let mut v = b"GIF89a".to_vec();
        v.extend_from_slice(&width.to_le_bytes());
        v.extend_from_slice(&height.to_le_bytes());
        v.extend_from_slice(&[0xF7, 0x00, 0x00]);
        v
    }

    /// A RIFF/WEBP container holding one chunk.
    fn webp(chunk: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&((payload.len() + 12) as u32).to_le_bytes());
        v.extend_from_slice(b"WEBP");
        v.extend_from_slice(chunk);
        v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        v.extend_from_slice(payload);
        v
    }

    /// A lossy WebP key frame declaring `width × height`.
    fn webp_lossy(width: u16, height: u16) -> Vec<u8> {
        let mut p = vec![0x00, 0x00, 0x00, 0x9D, 0x01, 0x2A];
        p.extend_from_slice(&width.to_le_bytes());
        p.extend_from_slice(&height.to_le_bytes());
        p.extend_from_slice(&[0, 0, 0, 0]);
        webp(b"VP8 ", &p)
    }

    /// A lossless WebP declaring `width × height` in its packed 14-bit fields.
    fn webp_lossless(width: u32, height: u32) -> Vec<u8> {
        let bits = (width - 1) | ((height - 1) << 14);
        let mut p = vec![0x2F];
        p.extend_from_slice(&bits.to_le_bytes());
        p.extend_from_slice(&[0, 0, 0, 0]);
        webp(b"VP8L", &p)
    }

    /// An extended WebP whose canvas is `width × height`.
    fn webp_extended(width: u32, height: u32) -> Vec<u8> {
        let mut p = vec![0x10, 0, 0, 0];
        p.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
        p.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
        webp(b"VP8X", &p)
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

    /// Every media type hrdr can attach, with a valid fixture and the MIME
    /// string it must put on the wire — the table the three dialect shape tests
    /// walk, so a mapping slip for a type no other test renders cannot pass.
    fn every_media_type() -> Vec<(MediaType, Vec<u8>, &'static str)> {
        vec![
            (MediaType::Png, png(1), "image/png"),
            (MediaType::Jpeg, jpeg_sized(8, 8), "image/jpeg"),
            (MediaType::Gif, gif_sized(8, 8), "image/gif"),
            (MediaType::Webp, webp_lossy(8, 8), "image/webp"),
            (MediaType::Pdf, pdf(1), "application/pdf"),
        ]
    }

    /// Five bytes that, appended to an 8-byte file header, make the standard
    /// base64 of the whole use every part of the alphabet: `+`, `/` and `=`
    /// padding all appear in the encodings below. A URL-safe or unpadded
    /// encoder produces none of the three.
    const ALPHABET_SUFFIX: [u8; 5] = [0xBF, 0x0F, 0x4F, 0x88, 0x2F];

    /// `STANDARD.encode(b"\x89PNG\r\n\x1a\n" ++ ALPHABET_SUFFIX)`.
    const ALPHABET_PNG_BASE64: &str = "iVBORw0KGgq/D0+ILw==";

    /// `STANDARD.encode(b"%PDF-1.7" ++ ALPHABET_SUFFIX)`.
    const ALPHABET_PDF_BASE64: &str = "JVBERi0xLje/D0+ILw==";

    /// A file header plus [`ALPHABET_SUFFIX`].
    fn alphabet_bytes(header: &[u8]) -> Vec<u8> {
        let mut v = header.to_vec();
        v.extend_from_slice(&ALPHABET_SUFFIX);
        v
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

    /// With nothing configured, the per-image cap is Anthropic's documented
    /// per-image size, measured on the **encoded** size, and it is a boundary:
    /// the largest image that fits passes, one byte more is refused, and the
    /// error names the limit.
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

        // A PDF the same size is fine — that default is Anthropic's *image*
        // limit; a PDF is bounded by the request budget instead. Sharing one
        // default would refuse a document the provider accepts.
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
            check_attachments("m", None, &msgs, Some(DEFAULT_MAX_IMAGE_BASE64_BYTES * 2)),
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

    /// The Anthropic block shapes, exactly as the Messages API spells them: an
    /// `image` block for each image type, a `document` block for a PDF, each
    /// carrying an inline `base64` source and **nothing else**.
    ///
    /// Compared whole, and that is the point of the assertion rather than a
    /// convenience: the Messages API's image block takes `type`/`source` (plus
    /// an optional `cache_control` that belongs to the caching layer, not
    /// here), so a stray `detail`, `filename` or `title` this renderer invented
    /// fails the comparison exactly as a missing `media_type` would. Same for
    /// the source object, which must be the base64 variant's three fields.
    #[test]
    fn anthropic_blocks_match_the_documented_shape() {
        for (media_type, bytes, mime) in every_media_type() {
            let a = Attachment::new(bytes.clone(), media_type, "attachment.bin").unwrap();
            let kind = if media_type == MediaType::Pdf {
                "document"
            } else {
                "image"
            };
            assert_eq!(
                a.anthropic_block(),
                json!({
                    "type": kind,
                    "source": {
                        "type": "base64",
                        "media_type": mime,
                        "data": STANDARD.encode(&bytes),
                    },
                }),
                "{media_type} block"
            );
        }
    }

    /// The Responses item shapes: `input_image` with `image_url` as a bare
    /// **string** (not the chat-completions object) and a sibling `detail`,
    /// `input_file` with the file name beside the payload. Compared whole, for
    /// the reason [`anthropic_blocks_match_the_documented_shape`] gives.
    #[test]
    fn responses_items_match_the_documented_shape() {
        for (media_type, bytes, mime) in every_media_type() {
            let a = Attachment::new(bytes.clone(), media_type, "attachment.bin").unwrap();
            let payload = format!("data:{mime};base64,{}", STANDARD.encode(&bytes));
            let expected = if media_type == MediaType::Pdf {
                json!({
                    "type": "input_file",
                    "filename": "attachment.bin",
                    "file_data": payload,
                })
            } else {
                json!({
                    "type": "input_image",
                    "image_url": payload,
                    "detail": "auto",
                })
            };
            assert_eq!(a.responses_item(), expected, "{media_type} item");
        }
    }

    /// The chat-completions part shapes: `image_url` as an **object** holding
    /// `url` + `detail`, `file` as an object holding `filename` + `file_data`
    /// (and no `file_id`, which names an upload this client never makes).
    /// Compared whole, for the reason
    /// [`anthropic_blocks_match_the_documented_shape`] gives.
    #[test]
    fn openai_parts_match_the_documented_shape() {
        for (media_type, bytes, mime) in every_media_type() {
            let a = Attachment::new(bytes.clone(), media_type, "attachment.bin").unwrap();
            let payload = format!("data:{mime};base64,{}", STANDARD.encode(&bytes));
            let expected = if media_type == MediaType::Pdf {
                json!({
                    "type": "file",
                    "file": { "filename": "attachment.bin", "file_data": payload },
                })
            } else {
                json!({
                    "type": "image_url",
                    "image_url": { "url": payload, "detail": "auto" },
                })
            };
            assert_eq!(a.openai_part(), expected, "{media_type} part");
        }
    }

    /// The payload form is per dialect and is not the same one twice: Anthropic
    /// takes **raw** base64 in `source.data`, both OpenAI dialects take a
    /// `data:<mime>;base64,` URL. The shape tests above build their expectation
    /// with the same encoder the renderers use, so they cannot see the encoder
    /// itself change; these literals can — the fixtures encode to `+`, `/` and
    /// `=` padding, none of which a URL-safe or unpadded engine emits.
    #[test]
    fn each_dialect_encodes_the_payload_the_way_it_documents() {
        let img = Attachment::new(
            alphabet_bytes(b"\x89PNG\r\n\x1a\n"),
            MediaType::Png,
            "a.png",
        )
        .unwrap();
        let doc = Attachment::new(alphabet_bytes(b"%PDF-1.7"), MediaType::Pdf, "a.pdf").unwrap();
        let img_url = format!("data:image/png;base64,{ALPHABET_PNG_BASE64}");
        let doc_url = format!("data:application/pdf;base64,{ALPHABET_PDF_BASE64}");

        // Anthropic: raw base64, no `data:` prefix.
        assert_eq!(
            img.anthropic_block()["source"]["data"],
            json!(ALPHABET_PNG_BASE64)
        );
        assert_eq!(
            doc.anthropic_block()["source"]["data"],
            json!(ALPHABET_PDF_BASE64)
        );
        // Responses and chat-completions: the data URL, mime included.
        assert_eq!(img.responses_item()["image_url"], json!(img_url));
        assert_eq!(doc.responses_item()["file_data"], json!(doc_url));
        assert_eq!(img.openai_part()["image_url"]["url"], json!(img_url));
        assert_eq!(doc.openai_part()["file"]["file_data"], json!(doc_url));
    }

    /// Every format's header is read for the exact dimensions it declares.
    /// Values chosen to be distinguishable from each other, so a width/height
    /// swap or a byte-order slip cannot pass.
    #[test]
    fn dimensions_are_read_from_each_format_header() {
        assert_eq!(
            image_dimensions(MediaType::Png, &png_sized(1500, 1000)),
            Some((1500, 1000))
        );
        assert_eq!(
            image_dimensions(MediaType::Jpeg, &jpeg_sized(1920, 1080)),
            Some((1920, 1080))
        );
        assert_eq!(
            image_dimensions(MediaType::Gif, &gif_sized(640, 480)),
            Some((640, 480))
        );
        assert_eq!(
            image_dimensions(MediaType::Webp, &webp_lossy(1024, 768)),
            Some((1024, 768))
        );
        assert_eq!(
            image_dimensions(MediaType::Webp, &webp_lossless(800, 600)),
            Some((800, 600))
        );
        assert_eq!(
            image_dimensions(MediaType::Webp, &webp_extended(3840, 2160)),
            Some((3840, 2160))
        );
        // The 14-bit fields are the whole width of the lossy/lossless formats:
        // the largest value each can hold must come back intact.
        assert_eq!(
            image_dimensions(MediaType::Webp, &webp_lossy(16383, 16383)),
            Some((16383, 16383))
        );
        assert_eq!(
            image_dimensions(MediaType::Webp, &webp_lossless(16384, 16384)),
            Some((16384, 16384))
        );
        // A PDF has no dimensions to read.
        assert_eq!(image_dimensions(MediaType::Pdf, &pdf(64)), None);
    }

    /// Truncated, empty and hostile headers parse as `None` and never panic —
    /// these bytes come off a clipboard or a path the user named.
    #[test]
    fn a_malformed_header_is_none_and_never_panics() {
        let fixtures = [
            (MediaType::Png, png_sized(64, 64)),
            (MediaType::Jpeg, jpeg_sized(64, 64)),
            (MediaType::Gif, gif_sized(64, 64)),
            (MediaType::Webp, webp_lossy(64, 64)),
            (MediaType::Webp, webp_lossless(64, 64)),
            (MediaType::Webp, webp_extended(64, 64)),
        ];
        for (media_type, bytes) in &fixtures {
            // Empty, and every truncation of a good header: no panic, and a cut
            // that removes the dimensions cannot still report them.
            assert_eq!(image_dimensions(*media_type, b""), None);
            for n in 0..bytes.len() {
                let _ = image_dimensions(*media_type, &bytes[..n]);
            }
            // Every single-byte corruption, to both extremes: no panic. A
            // hostile file is exactly a header whose lengths lie.
            for i in 0..bytes.len() {
                for poison in [0x00u8, 0xFF] {
                    let mut b = bytes.clone();
                    b[i] = poison;
                    let _ = image_dimensions(*media_type, &b);
                }
            }
        }

        // A zero edge is malformed, not a 0-token image.
        assert_eq!(image_dimensions(MediaType::Png, &png_sized(0, 100)), None);
        assert_eq!(image_dimensions(MediaType::Png, &png_sized(100, 0)), None);
        assert_eq!(image_dimensions(MediaType::Gif, &gif_sized(0, 0)), None);
        // PNG: a first chunk that is not IHDR.
        let mut not_ihdr = png_sized(10, 10);
        not_ihdr[12..16].copy_from_slice(b"gAMA");
        assert_eq!(image_dimensions(MediaType::Png, &not_ihdr), None);
        // JPEG: a segment claiming more bytes than the file holds.
        assert_eq!(
            image_dimensions(MediaType::Jpeg, &[0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xFF, 0x00]),
            None
        );
        // JPEG: a length below its own two bytes would leave the walk standing
        // still.
        assert_eq!(
            image_dimensions(MediaType::Jpeg, &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x00, 0x00]),
            None
        );
        // JPEG: entropy data begins at SOS, so a file with no frame header
        // before it has none.
        let mut sos_first = vec![0xFF, 0xD8, 0xFF, 0xDA];
        sos_first.extend_from_slice(&jpeg_sized(64, 64)[2..]);
        assert_eq!(image_dimensions(MediaType::Jpeg, &sos_first), None);
        // JPEG: a DHT is in the SOF marker range but is not a frame header, so
        // its table bytes must not be read as dimensions.
        assert_eq!(
            image_dimensions(
                MediaType::Jpeg,
                &[0xFF, 0xD8, 0xFF, 0xC4, 0x00, 0x09, 1, 2, 3, 4, 5, 6, 7]
            ),
            None
        );
        // WebP: a sub-format this does not read, and a VP8 chunk whose start
        // code is wrong.
        assert_eq!(
            image_dimensions(MediaType::Webp, &webp(b"ANIM", &[0; 16])),
            None
        );
        let mut bad_start = webp_lossy(64, 64);
        bad_start[23] = 0x00;
        assert_eq!(image_dimensions(MediaType::Webp, &bad_start), None);
    }

    /// Anthropic's published reference points, both tiers — the only numbers
    /// that can show the formula is transcribed correctly, since they come from
    /// the vendor rather than from this implementation.
    #[test]
    fn visual_tokens_match_the_published_reference_points() {
        let standard = |w, h| visual_tokens(w, h, STANDARD_MAX_EDGE_PX, STANDARD_MAX_IMAGE_TOKENS);
        // The high-resolution tier (Claude 4.7 and later): 2576 px on the long
        // edge, 4784 visual tokens. Not what an attachment is charged — see
        // `Attachment::estimated_tokens` — but the second set of published
        // numbers, and so the second check on the same arithmetic.
        let high_res = |w, h| visual_tokens(w, h, 2_576, 4_784);

        // Under both caps: the bare ⌈w/28⌉ × ⌈h/28⌉.
        assert_eq!(standard(1000, 1000), 1296);
        assert_eq!(high_res(1000, 1000), 1296);
        assert_eq!(standard(1092, 1092), 1521);
        assert_eq!(high_res(1092, 1092), 1521);
        // 1080p: over the standard tier's token cap even after the long-edge
        // downscale, and under the high-res tier's caps entirely.
        assert_eq!(standard(1920, 1080), 1560);
        assert_eq!(high_res(1920, 1080), 2691);
        // 4K: both tiers downscale, and the aspect ratio is 1080p's, so the
        // standard tier lands on the same figure.
        assert_eq!(standard(3840, 2160), 1560);
        assert_eq!(high_res(3840, 2160), 4784);

        // Neither tier can be talked over its ceiling, whichever edge is long.
        for (w, h) in [(8000, 8000), (12000, 400), (400, 12000)] {
            assert!(
                standard(w, h) <= STANDARD_MAX_IMAGE_TOKENS as u32,
                "{w}x{h}"
            );
            assert!(high_res(w, h) <= 4_784, "{w}x{h}");
        }
        // Rotating an image cannot change what it costs.
        for (w, h) in [(1920, 1080), (1500, 1000), (37, 4000)] {
            assert_eq!(standard(w, h), standard(h, w), "{w}x{h}");
        }
    }

    /// An attachment carries its own cost, computed once from the bytes: an
    /// image's visual tokens, and the tier ceiling for a header this cannot
    /// read — never zero, which is what let an image-heavy history look empty.
    #[test]
    fn an_attachment_estimates_its_own_tokens() {
        // Charged at the high-resolution tier (see `CHARGED_MAX_EDGE_PX`): both
        // sit under its 2576 px edge and 4784 token ceilings, so neither is
        // downscaled and each costs its raw patch count.
        // ⌈1500/28⌉ × ⌈1000/28⌉ = 54 × 36.
        let shot = Attachment::new(png_sized(1500, 1000), MediaType::Png, "shot.png").unwrap();
        assert_eq!(shot.estimated_tokens(), 54 * 36);
        // The published figure for 1920×1080 on the high-resolution tier — the
        // standard tier would charge 1560 for the same screenshot.
        assert_eq!(
            Attachment::new(jpeg_sized(1920, 1080), MediaType::Jpeg, "s.jpg")
                .unwrap()
                .estimated_tokens(),
            2691
        );

        // A PNG whose IHDR is filler rather than a real header: the fallback,
        // which is the most the tier can charge for one image.
        let unreadable = png_attachment("a.png");
        assert_eq!(unreadable.estimated_tokens(), UNKNOWN_IMAGE_TOKENS);
        assert!(unreadable.estimated_tokens() > 0);

        // A tiny image is one patch, not zero.
        assert_eq!(
            Attachment::new(png_sized(1, 1), MediaType::Png, "dot.png")
                .unwrap()
                .estimated_tokens(),
            1
        );
    }

    /// The two fallbacks behind the parser: the byte scan for a file with no
    /// cross-reference chain to follow, and the file's size for one that
    /// declares nothing at all. Never zero.
    #[test]
    fn a_pdf_is_charged_by_its_pages() {
        // A fragment of a page tree with no trailer, so nothing to parse: three
        // leaves under one `/Pages` node, which must not itself be counted.
        let mut doc = b"%PDF-1.7\n1 0 obj<</Type /Pages/Kids[2 0 R 3 0 R 4 0 R]/Count 3>>".to_vec();
        for _ in 0..3 {
            doc.extend_from_slice(b"\nobj<</Type /Page/Parent 1 0 R>>");
        }
        assert_eq!(pdf_page_count(&doc), 3);
        let a = Attachment::new(doc, MediaType::Pdf, "r.pdf").unwrap();
        assert_eq!(a.estimated_tokens(), 3 * PDF_TOKENS_PER_PAGE);

        // The spacing-free spelling, and names that merely start with `/Page`.
        assert_eq!(pdf_page_count(b"<</Type/Page>><</Type /PageLabels>>"), 1);

        // A compressed page tree declares nothing: the fallback is the file's
        // size, and a small PDF is still charged a page.
        let opaque = pdf(4);
        assert_eq!(pdf_page_count(&opaque), 0);
        assert_eq!(
            Attachment::new(opaque, MediaType::Pdf, "o.pdf")
                .unwrap()
                .estimated_tokens(),
            PDF_TOKENS_PER_PAGE
        );
        let big = Attachment::new(pdf(PDF_BYTES_PER_PAGE * 4), MediaType::Pdf, "b.pdf").unwrap();
        assert_eq!(big.estimated_tokens(), 5 * PDF_TOKENS_PER_PAGE);
    }

    /// A real page tree outranks both fallbacks — in the two directions they
    /// get it wrong.
    #[test]
    fn a_real_page_count_beats_the_scan_and_the_guess() {
        // Compressed: the scan sees no page objects at all, and the file is
        // small, so both fallbacks would charge a single page for eleven.
        let compressed = crate::pdf::fixtures::xref_stream_objstm(11, true);
        assert_eq!(pdf_page_count(&compressed), 0);
        assert!(compressed.len() < PDF_BYTES_PER_PAGE);
        assert_eq!(
            Attachment::new(compressed, MediaType::Pdf, "c.pdf")
                .unwrap()
                .estimated_tokens(),
            11 * PDF_TOKENS_PER_PAGE
        );

        // `/Type /Page` in a string and in a content stream: the scan counts
        // twelve pages in a three-page document.
        let noisy = crate::pdf::fixtures::page_text_in_content();
        assert_eq!(pdf_page_count(&noisy), 12);
        assert_eq!(
            Attachment::new(noisy, MediaType::Pdf, "n.pdf")
                .unwrap()
                .estimated_tokens(),
            3 * PDF_TOKENS_PER_PAGE
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
