//! How many pages a PDF declares, read from its page tree.
//!
//! [`page_count`] exists for one caller — the token estimate in
//! [`crate::media`] — and answers one question. It finds the cross-reference
//! section named by `startxref`, follows the `/Prev` chain back through any
//! incremental updates, resolves the trailer's `/Root` to the document catalog
//! and its `/Pages` to the root of the page tree, and returns that node's
//! `/Count`.
//!
//! Section numbers in this file are ISO 32000-1, as published in the freely
//! available *PDF 1.7 Reference*: §7.5.5 (file trailer), §7.5.4 (cross-reference
//! table), §7.5.8 (cross-reference stream), §7.5.7 (object streams), §7.7.2
//! (document catalog), §7.7.3.2 (page tree node).
//!
//! # Everything here fails the same way
//!
//! The input is a file a user attached, parsed in-process before anything is
//! sent anywhere. So there is exactly one failure mode: return `None` and let
//! the caller's size-derived estimate stand. Malformed, encrypted, truncated,
//! hostile, using a feature this does not implement, or merely over one of the
//! bounds below — all `None`. A page count that is wrong but plausible is worse
//! than an honest guess, because nothing downstream can tell the two apart.
//!
//! Deliberately not implemented, each falling back the same way: encrypted
//! files (`/Encrypt` — no attempt is made to decrypt), any stream filter other
//! than `/FlateDecode`, the TIFF predictor (`/Predictor 2`), and the
//! hybrid-reference `/XRefStm` (a hybrid file whose catalog is reachable only
//! through the cross-reference stream falls back).

use std::collections::{HashMap, HashSet};
use std::io::Read;

use flate2::read::{DeflateDecoder, ZlibDecoder};

/// How far back from the end of the file the trailing `startxref` is looked
/// for. §7.5.5 puts it in the last lines of a conforming file; the slack is for
/// producers that append junk after `%%EOF`.
const STARTXREF_TAIL_BYTES: usize = 2048;

/// Cross-reference sections followed through `/Prev` before giving up. An
/// incrementally-saved file grows one section per save; a file with more
/// revisions than this is over the bound, and over a bound means `None`.
const MAX_XREF_SECTIONS: usize = 32;

/// Subsections read from one classic cross-reference table (§7.5.4). Each is a
/// `start count` header, so an unbounded loop here is a file made of headers.
const MAX_XREF_SUBSECTIONS: usize = 4_096;

/// Cross-reference entries recorded across the whole `/Prev` chain — the cap on
/// the one map this parser builds, and so on its memory.
const MAX_XREF_ENTRIES: usize = 500_000;

/// Output ceiling for a single inflate. A few hundred bytes of deflate can name
/// gigabytes of output, and this runs on a file a user merely attached: the
/// decompressor is bounded rather than trusted.
const MAX_INFLATED_BYTES: usize = 16 * 1024 * 1024;

/// Indirect objects fetched while resolving root → catalog → page tree. The
/// chain is three objects deep plus the object streams containing them, so this
/// is slack, not a budget anything real spends.
const MAX_OBJECT_FETCHES: usize = 16;

/// Nesting depth for dictionaries, arrays and parenthesised strings — the
/// recursion bound, so a file of ten thousand `[` cannot overflow the stack.
const MAX_NESTING_DEPTH: usize = 16;

/// Elements parsed into one object's dictionaries and arrays combined. A page
/// tree root can carry a `/Kids` array with an entry per page, so this is well
/// above any real document while still bounding the allocation.
const MAX_OBJECT_ITEMS: usize = 100_000;

/// Bytes in one name object. §7.3.5 recommends implementations support at least
/// 127; anything longer is a file playing games.
const MAX_NAME_BYTES: usize = 256;

/// How far past a stream's start `endstream` is looked for when `/Length`
/// cannot be trusted (missing, indirect, or naming a range `endstream` does not
/// follow).
const MAX_STREAM_SCAN_BYTES: usize = 8 * 1024 * 1024;

/// Objects listed in one object stream's `/N`.
const MAX_OBJSTM_OBJECTS: usize = 100_000;

/// Columns accepted in a PNG predictor's `/DecodeParms`. A cross-reference
/// stream's row is the width of one entry — a dozen bytes — so this is only
/// here to keep the row buffer bounded.
const MAX_PREDICTOR_COLUMNS: usize = 1 << 20;

/// The largest `/Count` believed. Above this the file is claiming more pages
/// than any real document has, which makes it evidence of a broken parse rather
/// than a page count.
const MAX_PAGE_COUNT: u32 = 100_000;

/// Pages declared by `bytes`, or `None` if the file cannot be read with
/// certainty — see the module documentation for what "cannot" covers.
///
/// Never panics, never blocks, and never allocates more than the bounds in this
/// module allow, whatever the input.
pub(crate) fn page_count(bytes: &[u8]) -> Option<u32> {
    Document::load(bytes)?.pages()
}

// ---------------------------------------------------------------------------
// Objects (§7.3)
// ---------------------------------------------------------------------------

/// A PDF object.
///
/// Booleans, reals and strings keep no payload: nothing here reads one, and
/// the parser only needs to step *over* them correctly — a string can contain
/// anything, including `>>` and `/Type /Page`, which is precisely what defeats
/// a byte scan. Their *type* still matters, because `/Count (3)` is a string
/// and not a page count.
#[derive(Debug, Clone)]
enum Obj {
    Null,
    Bool,
    Int(i64),
    Real,
    Str,
    Name(Vec<u8>),
    Array(Vec<Obj>),
    Dict(Dict),
    /// An indirect reference (§7.3.10). The generation number is dropped: this
    /// parser resolves by object number, and a file whose generations disagree
    /// with its cross-reference table is one it will fail on either way.
    Ref(u32),
}

impl Obj {
    fn as_int(&self) -> Option<i64> {
        match self {
            Obj::Int(value) => Some(*value),
            _ => None,
        }
    }

    fn as_name(&self) -> Option<&[u8]> {
        match self {
            Obj::Name(name) => Some(name),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[Obj]> {
        match self {
            Obj::Array(items) => Some(items),
            _ => None,
        }
    }

    fn as_dict(&self) -> Option<&Dict> {
        match self {
            Obj::Dict(dict) => Some(dict),
            _ => None,
        }
    }

    fn into_dict(self) -> Option<Dict> {
        match self {
            Obj::Dict(dict) => Some(dict),
            _ => None,
        }
    }
}

/// A dictionary, in file order. Lookup is linear because these are small and a
/// hash map per dictionary would cost more than it saves.
#[derive(Debug, Clone)]
struct Dict(Vec<(Vec<u8>, Obj)>);

impl Dict {
    fn get(&self, key: &[u8]) -> Option<&Obj> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn int(&self, key: &[u8]) -> Option<i64> {
        self.get(key)?.as_int()
    }

    fn name(&self, key: &[u8]) -> Option<&[u8]> {
        self.get(key)?.as_name()
    }
}

// ---------------------------------------------------------------------------
// Lexer (§7.2)
// ---------------------------------------------------------------------------

/// The six white-space bytes of §7.2.3, Table 1.
const fn is_ws(b: u8) -> bool {
    matches!(b, 0 | 9 | 10 | 12 | 13 | 32)
}

/// The delimiters of §7.2.3, Table 2.
const fn is_delim(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

/// A regular character: neither white space nor a delimiter — what tokens are
/// made of.
const fn is_regular(b: u8) -> bool {
    !is_ws(b) && !is_delim(b)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// A cursor over PDF syntax.
///
/// Every read goes through `get`: there is no indexing and no slicing by an
/// unchecked range anywhere in this file, so a position derived from file
/// content can only ever produce `None`.
struct Lexer<'a> {
    bytes: &'a [u8],
    pos: usize,
    items: usize,
}

impl<'a> Lexer<'a> {
    fn at(bytes: &'a [u8], pos: usize) -> Option<Self> {
        (pos <= bytes.len()).then_some(Self {
            bytes,
            pos,
            items: 0,
        })
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// White space and comments (§7.2.4: `%` to the end of the line).
    fn skip_space(&mut self) {
        while let Some(b) = self.peek() {
            if is_ws(b) {
                self.pos += 1;
            } else if b == b'%' {
                while self.peek().is_some_and(|c| c != b'\n' && c != b'\r') {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    /// The next run of regular characters, or `None` at a delimiter or EOF.
    fn token(&mut self) -> Option<&'a [u8]> {
        self.skip_space();
        let start = self.pos;
        while self.peek().is_some_and(is_regular) {
            self.pos += 1;
        }
        self.bytes.get(start..self.pos).filter(|t| !t.is_empty())
    }

    /// Consumes `kw` if it is the next whole token.
    fn keyword(&mut self, kw: &[u8]) -> bool {
        let save = self.pos;
        if self.token() == Some(kw) {
            return true;
        }
        self.pos = save;
        false
    }

    /// The next token as an integer, leaving the position untouched when it is
    /// not one.
    fn integer(&mut self) -> Option<i64> {
        let save = self.pos;
        let parsed = self
            .token()
            .and_then(|t| std::str::from_utf8(t).ok())
            .and_then(|t| t.parse::<i64>().ok());
        if parsed.is_none() {
            self.pos = save;
        }
        parsed
    }

    /// Charges one collection element against [`MAX_OBJECT_ITEMS`].
    fn charge(&mut self) -> Option<()> {
        self.items += 1;
        (self.items <= MAX_OBJECT_ITEMS).then_some(())
    }

    /// One object at the current position.
    fn object(&mut self, depth: usize) -> Option<Obj> {
        if depth > MAX_NESTING_DEPTH {
            return None;
        }
        self.skip_space();
        match self.peek()? {
            b'<' => {
                if self.bytes.get(self.pos + 1) == Some(&b'<') {
                    self.pos += 2;
                    self.dict(depth)
                } else {
                    self.hex_string()
                }
            }
            b'[' => {
                self.pos += 1;
                self.array(depth)
            }
            b'/' => self.name().map(Obj::Name),
            b'(' => self.literal_string(),
            b'0'..=b'9' | b'+' | b'-' | b'.' => self.number_or_ref(),
            b')' | b'>' | b']' | b'{' | b'}' => None,
            _ => match self.token()? {
                b"true" | b"false" => Some(Obj::Bool),
                b"null" => Some(Obj::Null),
                _ => None,
            },
        }
    }

    /// A name object (§7.3.5), with `#xx` escapes resolved.
    fn name(&mut self) -> Option<Vec<u8>> {
        self.skip_space();
        if self.peek()? != b'/' {
            return None;
        }
        self.pos += 1;
        let mut out = Vec::new();
        while let Some(b) = self.peek().filter(|b| is_regular(*b)) {
            self.pos += 1;
            if b == b'#' {
                let hi = hex_val(self.peek()?)?;
                self.pos += 1;
                let lo = hex_val(self.peek()?)?;
                self.pos += 1;
                out.push(hi * 16 + lo);
            } else {
                out.push(b);
            }
            if out.len() > MAX_NAME_BYTES {
                return None;
            }
        }
        Some(out)
    }

    /// A literal string (§7.3.4.2): balanced parentheses, `\` escaping the byte
    /// after it. The content is stepped over, never kept.
    fn literal_string(&mut self) -> Option<Obj> {
        if self.peek()? != b'(' {
            return None;
        }
        self.pos += 1;
        let mut depth = 1usize;
        while let Some(b) = self.peek() {
            self.pos += 1;
            match b {
                b'\\' => {
                    self.peek()?;
                    self.pos += 1;
                }
                b'(' => {
                    depth += 1;
                    if depth > MAX_NESTING_DEPTH {
                        return None;
                    }
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(Obj::Str);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// A hexadecimal string (§7.3.4.3), stepped over the same way.
    fn hex_string(&mut self) -> Option<Obj> {
        if self.peek()? != b'<' {
            return None;
        }
        self.pos += 1;
        while let Some(b) = self.peek() {
            self.pos += 1;
            if b == b'>' {
                return Some(Obj::Str);
            }
            if !b.is_ascii_hexdigit() && !is_ws(b) {
                return None;
            }
        }
        None
    }

    /// A number, or the `N G R` indirect reference that begins with one
    /// (§7.3.10) — the lookahead is unwound when the `R` does not arrive.
    fn number_or_ref(&mut self) -> Option<Obj> {
        let text = std::str::from_utf8(self.token()?).ok()?;
        if let Ok(value) = text.parse::<i64>() {
            let after_number = self.pos;
            if let Ok(num) = u32::try_from(value)
                && let Some(generation) = self.integer()
                && (0..=65_535).contains(&generation)
                && self.keyword(b"R")
            {
                return Some(Obj::Ref(num));
            }
            self.pos = after_number;
            return Some(Obj::Int(value));
        }
        // A real (§7.3.3). Nothing here reads one, but it has to parse for the
        // dictionary containing it to parse.
        if text.is_empty()
            || !text
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'+' | b'-' | b'.'))
        {
            return None;
        }
        text.parse::<f64>().ok().filter(|v| v.is_finite())?;
        Some(Obj::Real)
    }

    fn array(&mut self, depth: usize) -> Option<Obj> {
        let mut items = Vec::new();
        loop {
            self.skip_space();
            if self.peek()? == b']' {
                self.pos += 1;
                return Some(Obj::Array(items));
            }
            self.charge()?;
            items.push(self.object(depth + 1)?);
        }
    }

    fn dict(&mut self, depth: usize) -> Option<Obj> {
        let mut entries = Vec::new();
        loop {
            self.skip_space();
            if self.peek()? == b'>' {
                if self.bytes.get(self.pos + 1) != Some(&b'>') {
                    return None;
                }
                self.pos += 2;
                return Some(Obj::Dict(Dict(entries)));
            }
            self.charge()?;
            let key = self.name()?;
            let value = self.object(depth + 1)?;
            entries.push((key, value));
        }
    }
}

// ---------------------------------------------------------------------------
// Indirect objects and streams (§7.3.8, §7.3.10)
// ---------------------------------------------------------------------------

/// The indirect object at `at`: its number, its value, and the raw — still
/// encoded — bytes of its stream if it has one.
fn indirect_at(bytes: &[u8], at: usize) -> Option<(u32, Obj, Option<&[u8]>)> {
    let mut lx = Lexer::at(bytes, at)?;
    let num = u32::try_from(lx.integer()?).ok()?;
    let _generation = lx.integer()?;
    if !lx.keyword(b"obj") {
        return None;
    }
    let value = lx.object(0)?;
    let stream = if lx.keyword(b"stream") {
        Some(stream_bytes(bytes, value.as_dict()?, lx.pos)?)
    } else {
        None
    };
    Some((num, value, stream))
}

/// The raw bytes of the stream whose `stream` keyword ends at `after_keyword`.
fn stream_bytes<'a>(bytes: &'a [u8], dict: &Dict, after_keyword: usize) -> Option<&'a [u8]> {
    // §7.3.8.1: the keyword is followed by CRLF or a single LF. A lone CR is
    // malformed, and is tolerated here because producers emit it.
    let mut start = after_keyword;
    match bytes.get(start) {
        Some(b'\r') => {
            start += 1;
            if bytes.get(start) == Some(&b'\n') {
                start += 1;
            }
        }
        Some(b'\n') => start += 1,
        _ => {}
    }
    let rest = bytes.get(start..)?;
    // `/Length` is believed only when `endstream` really follows the range it
    // names: it is often an indirect reference — unresolvable here, since the
    // cross-reference table may be the very thing being read — and is
    // sometimes simply wrong.
    if let Some(len) = dict.int(b"Length").and_then(|v| usize::try_from(v).ok())
        && let Some(data) = rest.get(..len)
        && let Some(tail) = rest.get(len..)
        && let Some(mut lx) = Lexer::at(tail, 0)
        && lx.keyword(b"endstream")
    {
        return Some(data);
    }
    // Otherwise the data ends at the next `endstream`, less the EOL before it.
    let window = rest.get(..rest.len().min(MAX_STREAM_SCAN_BYTES))?;
    let mut end = find(window, b"endstream")?;
    if end > 0 && rest.get(end - 1) == Some(&b'\n') {
        end -= 1;
    }
    if end > 0 && rest.get(end - 1) == Some(&b'\r') {
        end -= 1;
    }
    rest.get(..end)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

/// Inflates `data`, refusing anything that expands past [`MAX_INFLATED_BYTES`].
fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    fn bounded(reader: impl Read) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        // One byte over the ceiling is read on purpose: it is what tells
        // "exactly at the ceiling" apart from "truncated at it".
        let ceiling = u64::try_from(MAX_INFLATED_BYTES).ok()?.checked_add(1)?;
        reader.take(ceiling).read_to_end(&mut out).ok()?;
        (out.len() <= MAX_INFLATED_BYTES).then_some(out)
    }
    // §7.4.4: `/FlateDecode` is zlib (RFC 1950). Files from broken producers
    // carry a bare deflate stream (RFC 1951) instead, which is worth the retry.
    bounded(ZlibDecoder::new(data)).or_else(|| bounded(DeflateDecoder::new(data)))
}

/// A stream's data with its filter and predictor undone.
fn decode_stream(dict: &Dict, raw: &[u8]) -> Option<Vec<u8>> {
    const FLATE: &[u8] = b"FlateDecode";
    let filters: Vec<&[u8]> = match dict.get(b"Filter") {
        None | Some(Obj::Null) => Vec::new(),
        Some(Obj::Name(name)) => vec![name.as_slice()],
        Some(Obj::Array(items)) => items.iter().map(Obj::as_name).collect::<Option<_>>()?,
        Some(_) => return None,
    };
    let data = match filters.as_slice() {
        [] => raw.to_vec(),
        [only] if *only == FLATE => inflate(raw)?,
        // Any other filter chain — one this does not implement, or one it has
        // never heard of — is a fallback, not a guess.
        _ => return None,
    };

    let parms = match dict.get(b"DecodeParms") {
        None | Some(Obj::Null) => return Some(data),
        Some(Obj::Dict(parms)) => parms,
        // Parallel to `/Filter`: with one filter there is one parameter
        // dictionary, and the array may pad with nulls.
        Some(Obj::Array(items)) => match items.iter().find_map(Obj::as_dict) {
            Some(parms) => parms,
            None => return Some(data),
        },
        Some(_) => return None,
    };
    match parms.int(b"Predictor").unwrap_or(1) {
        predictor if predictor <= 1 => Some(data),
        // §7.4.4.4: predictor values ≥ 10 are the PNG filters, which prefix
        // every row with the filter byte that produced it.
        predictor if predictor >= 10 => {
            let colors = usize::try_from(parms.int(b"Colors").unwrap_or(1)).ok()?;
            let bpc = usize::try_from(parms.int(b"BitsPerComponent").unwrap_or(8)).ok()?;
            let columns = usize::try_from(parms.int(b"Columns").unwrap_or(1)).ok()?;
            undo_png_predictor(&data, colors, bpc, columns)
        }
        // The TIFF predictor (2) is not implemented. Falling back beats
        // returning bytes that decode into plausible, wrong offsets.
        _ => None,
    }
}

/// RFC 2083 §6.6, the Paeth predictor.
fn paeth(left: u8, above: u8, upper_left: u8) -> u8 {
    let (a, b, c) = (i16::from(left), i16::from(above), i16::from(upper_left));
    let p = a + b - c;
    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
    if pa <= pb && pa <= pc {
        left
    } else if pb <= pc {
        above
    } else {
        upper_left
    }
}

/// Undoes the PNG row filters of RFC 2083 §6, as referenced by §7.4.4.4.
///
/// `data` is rows of `filter byte + row bytes`; a length that is not a whole
/// number of such rows means the parameters were misread, and misread
/// parameters yield garbage rather than an error — so that is a `None`.
fn undo_png_predictor(data: &[u8], colors: usize, bpc: usize, columns: usize) -> Option<Vec<u8>> {
    if !(1..=32).contains(&colors)
        || !matches!(bpc, 1 | 2 | 4 | 8 | 16)
        || !(1..=MAX_PREDICTOR_COLUMNS).contains(&columns)
    {
        return None;
    }
    let bpp = (colors * bpc).div_ceil(8).max(1);
    let row_len = (colors * bpc * columns).div_ceil(8);
    if row_len == 0 || data.is_empty() || !data.len().is_multiple_of(row_len + 1) {
        return None;
    }
    let rows = data.len() / (row_len + 1);
    let mut out = vec![0u8; rows * row_len];
    for row in 0..rows {
        let filter = *data.get(row * (row_len + 1))?;
        for col in 0..row_len {
            let raw = *data.get(row * (row_len + 1) + 1 + col)?;
            let left = match col.checked_sub(bpp) {
                Some(prev) => *out.get(row * row_len + prev)?,
                None => 0,
            };
            let (above, upper_left) = match row.checked_sub(1) {
                Some(above_row) => (
                    *out.get(above_row * row_len + col)?,
                    match col.checked_sub(bpp) {
                        Some(prev) => *out.get(above_row * row_len + prev)?,
                        None => 0,
                    },
                ),
                None => (0, 0),
            };
            let value = match filter {
                0 => raw,
                1 => raw.wrapping_add(left),
                2 => raw.wrapping_add(above),
                3 => raw.wrapping_add(((u16::from(left) + u16::from(above)) / 2) as u8),
                4 => raw.wrapping_add(paeth(left, above, upper_left)),
                _ => return None,
            };
            *out.get_mut(row * row_len + col)? = value;
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Cross-reference sections (§7.5.4, §7.5.8) and the document
// ---------------------------------------------------------------------------

/// Where one object lives, as a cross-reference section says.
#[derive(Debug, Clone, Copy)]
enum XrefEntry {
    /// Free (§7.5.4) — the object is not in the file.
    Free,
    /// A byte offset from the start of the file.
    Offset(usize),
    /// Inside the object stream `stream`, at position `index` (§7.5.8.3,
    /// type 2).
    InStream { stream: u32, index: usize },
}

struct Document<'a> {
    bytes: &'a [u8],
    xref: HashMap<u32, XrefEntry>,
    root: Option<u32>,
    fetches: usize,
}

impl<'a> Document<'a> {
    /// Reads the cross-reference chain: the section `startxref` names, then
    /// each `/Prev` behind it.
    fn load(bytes: &'a [u8]) -> Option<Self> {
        let mut doc = Document {
            bytes,
            xref: HashMap::new(),
            root: None,
            fetches: 0,
        };
        let mut offset = startxref(bytes)?;
        let mut seen = HashSet::new();
        for _ in 0..MAX_XREF_SECTIONS {
            // A `/Prev` naming an offset already read — itself, most simply —
            // is a loop, and a loop here is a hang on a file the user merely
            // attached.
            if !seen.insert(offset) {
                return None;
            }
            let trailer = doc.read_section(offset)?;
            // §7.6: an encrypted document's strings and streams are ciphertext.
            // Nothing here decrypts, so this is where it stops.
            if trailer.get(b"Encrypt").is_some() {
                return None;
            }
            // §7.5.5: the newest section's `/Root` is the one that counts, and
            // sections are read newest first.
            if doc.root.is_none()
                && let Some(Obj::Ref(num)) = trailer.get(b"Root")
            {
                doc.root = Some(*num);
            }
            match trailer.get(b"Prev") {
                None => return Some(doc),
                Some(Obj::Int(prev)) => offset = usize::try_from(*prev).ok()?,
                Some(_) => return None,
            }
        }
        None
    }

    /// One cross-reference section, in whichever of the two forms it is
    /// written, with its entries merged into the map. Returns its trailer.
    fn read_section(&mut self, at: usize) -> Option<Dict> {
        let mut lx = Lexer::at(self.bytes, at)?;
        if lx.keyword(b"xref") {
            self.read_table(&mut lx)
        } else {
            self.read_xref_stream(at)
        }
    }

    /// Records an entry unless a newer section already placed that object —
    /// sections are read newest first, so first seen wins.
    fn record(&mut self, num: u32, entry: XrefEntry) -> Option<()> {
        if self.xref.len() >= MAX_XREF_ENTRIES {
            return None;
        }
        self.xref.entry(num).or_insert(entry);
        Some(())
    }

    /// The classic cross-reference table of §7.5.4: `start count` subsection
    /// headers, one entry per object, then the `trailer` dictionary.
    fn read_table(&mut self, lx: &mut Lexer<'a>) -> Option<Dict> {
        for _ in 0..MAX_XREF_SUBSECTIONS {
            if lx.keyword(b"trailer") {
                return lx.object(0)?.into_dict();
            }
            let start = u32::try_from(lx.integer()?).ok()?;
            let count = u32::try_from(lx.integer()?).ok()?;
            for i in 0..count {
                let offset = lx.integer()?;
                let _generation = lx.integer()?;
                let entry = match lx.token()? {
                    b"n" => XrefEntry::Offset(usize::try_from(offset).ok()?),
                    b"f" => XrefEntry::Free,
                    _ => return None,
                };
                self.record(start.checked_add(i)?, entry)?;
            }
        }
        None
    }

    /// The cross-reference stream of §7.5.8, which replaced the table and
    /// carries the trailer's keys in its own stream dictionary — a file using
    /// one has no `trailer` keyword anywhere.
    fn read_xref_stream(&mut self, at: usize) -> Option<Dict> {
        let (_, value, raw) = indirect_at(self.bytes, at)?;
        let dict = value.into_dict()?;
        if dict.name(b"Type") != Some(b"XRef") {
            return None;
        }
        let data = decode_stream(&dict, raw?)?;

        // §7.5.8.2: `/W` gives the byte width of each of the three fields.
        let widths = dict.get(b"W")?.as_array()?;
        let mut w = [0usize; 3];
        if widths.len() < w.len() {
            return None;
        }
        for (slot, field) in w.iter_mut().zip(widths) {
            let width = usize::try_from(field.as_int()?).ok()?;
            if width > 8 {
                return None;
            }
            *slot = width;
        }
        let entry_len = w[0] + w[1] + w[2];
        if entry_len == 0 {
            return None;
        }

        // `/Index` defaults to `[0 /Size]` (§7.5.8.2).
        let index: Vec<i64> = match dict.get(b"Index") {
            None => vec![0, dict.int(b"Size")?],
            Some(Obj::Array(items)) => items.iter().map(Obj::as_int).collect::<Option<_>>()?,
            Some(_) => return None,
        };
        if !index.len().is_multiple_of(2) {
            return None;
        }

        let mut pos = 0usize;
        for pair in index.chunks_exact(2) {
            let [first, count] = pair else { return None };
            let start = u32::try_from(*first).ok()?;
            let count = u32::try_from(*count).ok()?;
            for i in 0..count {
                let row = data.get(pos..pos.checked_add(entry_len)?)?;
                pos += entry_len;
                // §7.5.8.2: a `/W` width of zero means the field is absent and
                // takes its default, which for the type field is 1.
                let kind = if w[0] == 0 {
                    1
                } else {
                    be_field(row.get(..w[0])?)
                };
                let second = be_field(row.get(w[0]..w[0] + w[1])?);
                let third = be_field(row.get(w[0] + w[1]..)?);
                let entry = match kind {
                    1 => XrefEntry::Offset(usize::try_from(second).ok()?),
                    2 => XrefEntry::InStream {
                        stream: u32::try_from(second).ok()?,
                        index: usize::try_from(third).ok()?,
                    },
                    // Type 0 is free; §7.5.8.3 says any other type shall be
                    // read as a reference to the null object — absent either
                    // way.
                    _ => XrefEntry::Free,
                };
                self.record(start.checked_add(i)?, entry)?;
            }
        }
        Some(dict)
    }

    /// Charges one object fetch against [`MAX_OBJECT_FETCHES`].
    fn charge_fetch(&mut self) -> Option<()> {
        self.fetches += 1;
        (self.fetches <= MAX_OBJECT_FETCHES).then_some(())
    }

    /// The object numbered `num`, from wherever the cross-reference map puts
    /// it.
    fn object(&mut self, num: u32) -> Option<Obj> {
        self.charge_fetch()?;
        match *self.xref.get(&num)? {
            XrefEntry::Free => None,
            XrefEntry::Offset(at) => {
                let (found, value, _) = indirect_at(self.bytes, at)?;
                // An offset naming some other object means the table and the
                // body disagree, and nothing read through it can be trusted.
                (found == num).then_some(value)
            }
            XrefEntry::InStream { stream, index } => self.object_in_stream(stream, index, num),
        }
    }

    /// An object stored inside an object stream (§7.5.7): the container's data
    /// is a table of `object-number offset` pairs followed by the objects
    /// themselves, all of it compressed.
    fn object_in_stream(&mut self, container: u32, index: usize, num: u32) -> Option<Obj> {
        self.charge_fetch()?;
        // §7.5.7: an object stream is itself a top-level indirect object, so a
        // type-2 entry pointing at another type-2 entry is malformed — and
        // requiring that is also what makes recursion here impossible.
        let XrefEntry::Offset(at) = *self.xref.get(&container)? else {
            return None;
        };
        let (found, value, raw) = indirect_at(self.bytes, at)?;
        if found != container {
            return None;
        }
        let dict = value.into_dict()?;
        if dict.name(b"Type") != Some(b"ObjStm") {
            return None;
        }
        let data = decode_stream(&dict, raw?)?;
        let count = usize::try_from(dict.int(b"N")?).ok()?;
        let first = usize::try_from(dict.int(b"First")?).ok()?;
        if count > MAX_OBJSTM_OBJECTS || index >= count {
            return None;
        }
        let mut pairs = Lexer::at(&data, 0)?;
        let mut found_at = None;
        for i in 0..=index {
            let number = pairs.integer()?;
            let relative = pairs.integer()?;
            if i == index {
                found_at = Some((number, usize::try_from(relative).ok()?));
            }
        }
        let (number, relative) = found_at?;
        if number != i64::from(num) {
            return None;
        }
        Lexer::at(&data, first.checked_add(relative)?)?.object(0)
    }

    /// `/Root` → the catalog's `/Pages` → that node's `/Count`.
    fn pages(&mut self) -> Option<u32> {
        let root = self.root?;
        let catalog = self.object(root)?.into_dict()?;
        // §7.7.2, Table 28: `/Pages` is required, and shall be an indirect
        // reference to the page tree's root node.
        let Some(Obj::Ref(tree)) = catalog.get(b"Pages") else {
            return None;
        };
        let pages = self.object(*tree)?.into_dict()?;
        // §7.7.3.2, Table 29: `/Count` on the root node is the number of leaf
        // nodes in the entire tree — the page count, without walking `/Kids`.
        let count = match pages.get(b"Count")? {
            Obj::Int(count) => *count,
            Obj::Ref(num) => self.object(*num)?.as_int()?,
            _ => return None,
        };
        let count = u32::try_from(count).ok()?;
        // A document has at least one page (§7.7.3.3), and a count above the
        // ceiling is evidence of a bad parse rather than a long document.
        (1..=MAX_PAGE_COUNT).contains(&count).then_some(count)
    }
}

/// A big-endian integer of up to eight bytes.
fn be_field(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
}

/// The offset the trailing `startxref` names (§7.5.5).
fn startxref(bytes: &[u8]) -> Option<usize> {
    const KEYWORD: &[u8] = b"startxref";
    let from = bytes.len().saturating_sub(STARTXREF_TAIL_BYTES);
    let tail = bytes.get(from..)?;
    let at = from + rfind(tail, KEYWORD)? + KEYWORD.len();
    let offset = usize::try_from(Lexer::at(bytes, at)?.integer()?).ok()?;
    // An offset at or past EOF cannot name a cross-reference section.
    (offset < bytes.len()).then_some(offset)
}

/// PDFs assembled byte by byte.
///
/// Every fixture here is a real file — a reader would open it — because the
/// only way to test a parser of a format is to hand it the format. They sit
/// outside `mod tests` so that [`crate::media`]'s tests can use them too: the
/// case that matters most over there is the one where the byte scan and this
/// parser disagree.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::collections::BTreeMap;
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::ZlibEncoder;

    pub(crate) fn deflate(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).expect("in-memory write");
        encoder.finish().expect("in-memory flush")
    }

    /// Assembles a PDF while recording where each object landed — which is the
    /// whole difficulty of writing one by hand, since the cross-reference
    /// section is a list of byte offsets into everything before it.
    pub(crate) struct Builder {
        bytes: Vec<u8>,
        offsets: BTreeMap<u32, usize>,
    }

    impl Builder {
        pub(crate) fn new() -> Self {
            Self {
                bytes: b"%PDF-1.7\n".to_vec(),
                offsets: BTreeMap::new(),
            }
        }

        pub(crate) fn len(&self) -> usize {
            self.bytes.len()
        }

        pub(crate) fn offset(&self, num: u32) -> usize {
            *self.offsets.get(&num).expect("object was written")
        }

        pub(crate) fn obj(&mut self, num: u32, body: &str) {
            self.offsets.insert(num, self.bytes.len());
            self.bytes
                .extend_from_slice(format!("{num} 0 obj\n{body}\nendobj\n").as_bytes());
        }

        /// A stream object, with a correct `/Length` appended to `dict`.
        pub(crate) fn stream(&mut self, num: u32, dict: &str, data: &[u8]) {
            self.stream_verbatim(num, &format!("{dict}/Length {}", data.len()), data);
        }

        /// A stream object whose dictionary is written exactly as given — the
        /// hook for the `/Length` values real files get wrong.
        pub(crate) fn stream_verbatim(&mut self, num: u32, dict: &str, data: &[u8]) {
            self.offsets.insert(num, self.bytes.len());
            self.bytes
                .extend_from_slice(format!("{num} 0 obj\n<<{dict}>>\nstream\n").as_bytes());
            self.bytes.extend_from_slice(data);
            self.bytes.extend_from_slice(b"\nendstream\nendobj\n");
        }

        /// A classic cross-reference table covering objects `1..=max` behind
        /// the free head entry, then the trailer (§7.5.4). Returns its offset.
        pub(crate) fn xref_table(&mut self, trailer_extra: &str) -> usize {
            let at = self.bytes.len();
            let max = *self.offsets.keys().max().expect("an object was written");
            let mut section = format!("xref\n0 {}\n0000000000 65535 f \n", max + 1);
            for num in 1..=max {
                match self.offsets.get(&num) {
                    // The 20-byte entry of §7.5.4: offset, generation, keyword.
                    Some(offset) => section.push_str(&format!("{offset:010} 00000 n \n")),
                    None => section.push_str("0000000000 65535 f \n"),
                }
            }
            section.push_str(&format!(
                "trailer\n<</Size {}/Root 1 0 R{trailer_extra}>>\n",
                max + 1
            ));
            self.bytes.extend_from_slice(section.as_bytes());
            at
        }

        pub(crate) fn finish(&mut self, startxref: usize) -> Vec<u8> {
            self.bytes
                .extend_from_slice(format!("startxref\n{startxref}\n%%EOF\n").as_bytes());
            self.bytes.clone()
        }
    }

    /// `/W [1 4 2]` cross-reference stream rows (§7.5.8.3): a type byte, a
    /// four-byte second field, a two-byte third.
    pub(crate) fn xref_rows(entries: &[(u8, usize, u16)]) -> Vec<u8> {
        let mut rows = Vec::new();
        for (kind, second, third) in entries {
            rows.push(*kind);
            rows.extend_from_slice(
                &u32::try_from(*second)
                    .expect("fixture offsets are small")
                    .to_be_bytes(),
            );
            rows.extend_from_slice(&third.to_be_bytes());
        }
        rows
    }

    /// The uncompressed body of an object stream holding `objects`, with the
    /// `/First` offset it needs: §7.5.7 puts `/N` pairs of
    /// `object-number offset` in front of the objects themselves, the offsets
    /// being relative to `/First`.
    pub(crate) fn objstm_payload(objects: &[(u32, String)]) -> (Vec<u8>, usize) {
        let mut pairs = String::new();
        let mut body = String::new();
        for (num, text) in objects {
            pairs.push_str(&format!("{num} {} ", body.len()));
            body.push_str(text);
            body.push(' ');
        }
        let first = pairs.len();
        (format!("{pairs}{body}").into_bytes(), first)
    }

    /// The catalog every fixture here uses, pointing at object 2.
    pub(crate) const CATALOG: &str = "<</Type/Catalog/Pages 2 0 R>>";

    /// `pages` page objects under one tree node, a classic table, nothing
    /// compressed: a PDF as they were written before 1.5.
    pub(crate) fn classic(pages: u32) -> Vec<u8> {
        let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", i + 3)).collect();
        let mut b = Builder::new();
        b.obj(1, "<</Type/Catalog/Pages 2 0 R>>");
        b.obj(
            2,
            &format!("<</Type/Pages/Kids[{}]/Count {pages}>>", kids.join(" ")),
        );
        for i in 0..pages {
            b.obj(i + 3, "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]>>");
        }
        let at = b.xref_table("");
        b.finish(at)
    }

    /// One page, but with `/Count` written verbatim — the hook for the counts
    /// a file has no business declaring.
    pub(crate) fn classic_with_count(count: &str) -> Vec<u8> {
        let mut b = Builder::new();
        b.obj(1, "<</Type/Catalog/Pages 2 0 R>>");
        b.obj(2, &format!("<</Type/Pages/Kids[3 0 R]/Count {count}>>"));
        b.obj(3, "<</Type/Page/Parent 2 0 R>>");
        let at = b.xref_table("");
        b.finish(at)
    }

    /// A one-page file whose trailer declares `/Encrypt` (§7.6): its strings
    /// and streams are ciphertext, and nothing here decrypts.
    pub(crate) fn encrypted() -> Vec<u8> {
        let mut b = Builder::new();
        b.obj(1, "<</Type/Catalog/Pages 2 0 R>>");
        b.obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>");
        b.obj(3, "<</Type/Page/Parent 2 0 R>>");
        b.obj(4, "<</Filter/Standard/V 2/R 3/Length 128>>");
        let at = b.xref_table("/Encrypt 4 0 R/ID[<0badc0de><0badc0de>]");
        b.finish(at)
    }

    /// Three real pages, plus `/Type /Page` in the two places a byte scan
    /// cannot tell from a page object: inside a literal string, and inside a
    /// content stream.
    pub(crate) fn page_text_in_content() -> Vec<u8> {
        let mut b = Builder::new();
        b.obj(1, "<</Type/Catalog/Pages 2 0 R>>");
        b.obj(2, "<</Type/Pages/Kids[3 0 R 4 0 R 5 0 R]/Count 3>>");
        for n in 3..=5 {
            b.obj(
                n,
                &format!("<</Type/Page/Parent 2 0 R/Contents {} 0 R>>", n + 3),
            );
        }
        for n in 6..=8 {
            b.stream(
                n,
                "",
                b"BT (/Type /Page and /Type /Page again) Tj ET\n% /Type /Page\n",
            );
        }
        let at = b.xref_table("");
        b.finish(at)
    }

    /// A three-page file, and the same file after an incremental update
    /// (§7.5.6) replacing its page tree with a five-page one. The two
    /// revisions declare different counts on purpose: a parser that follows
    /// `/Prev` to the older section, or merges the two the wrong way round,
    /// answers 3 where the file says 5.
    pub(crate) fn incremental() -> (Vec<u8>, Vec<u8>) {
        let mut b = Builder::new();
        b.obj(1, "<</Type/Catalog/Pages 2 0 R>>");
        b.obj(2, "<</Type/Pages/Kids[3 0 R 4 0 R 5 0 R]/Count 3>>");
        for n in 3..=5 {
            b.obj(n, "<</Type/Page/Parent 2 0 R>>");
        }
        let base_at = b.xref_table("");
        let base = b.finish(base_at);

        let mut updated = base.clone();
        let mut append = |text: &str| {
            let at = updated.len();
            updated.extend_from_slice(text.as_bytes());
            at
        };
        let two = append(
            "2 0 obj\n<</Type/Pages/Kids[3 0 R 4 0 R 5 0 R 6 0 R 7 0 R]/Count 5>>\nendobj\n",
        );
        let six = append("6 0 obj\n<</Type/Page/Parent 2 0 R>>\nendobj\n");
        let seven = append("7 0 obj\n<</Type/Page/Parent 2 0 R>>\nendobj\n");
        let at = updated.len();
        updated.extend_from_slice(
            format!(
                "xref\n2 1\n{two:010} 00000 n \n6 2\n{six:010} 00000 n \n{seven:010} 00000 n \n\
                 trailer\n<</Size 8/Root 1 0 R/Prev {base_at}>>\nstartxref\n{at}\n%%EOF\n"
            )
            .as_bytes(),
        );
        (base, updated)
    }

    /// PNG-encodes `rows` with the `Up` filter (RFC 2083 §6.3), which is what
    /// producers use on a cross-reference stream: every row is the byte-wise
    /// difference from the one above it, behind a filter byte of 2.
    fn png_up(rows: &[u8], row_len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut previous = vec![0u8; row_len];
        for row in rows.chunks(row_len) {
            out.push(2);
            for (i, byte) in row.iter().enumerate() {
                out.push(byte.wrapping_sub(previous[i]));
            }
            previous = row.to_vec();
        }
        out
    }

    /// A PDF 1.5-shaped file: a cross-reference *stream*, with the catalog,
    /// the page tree root and every page object stored inside a compressed
    /// object stream. Not one of `/Type /Catalog`, `/Type /Pages`,
    /// `/Type /Page` or `/Count` appears in the file's bytes — this is the
    /// document a byte scan is blind to, and it is how most PDFs written this
    /// decade are laid out.
    ///
    /// With `predictor`, the cross-reference stream is PNG-predicted and its
    /// `/Index` is written out; without, it is plain Flate with `/Index`
    /// defaulted to `[0 /Size]`. Both shapes occur in the wild.
    pub(crate) fn xref_stream_objstm(pages: u32, predictor: bool) -> Vec<u8> {
        let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", i + 5)).collect();
        let mut objects = vec![
            (1, CATALOG.to_string()),
            (
                2,
                format!("<</Type/Pages/Kids[{}]/Count {pages}>>", kids.join(" ")),
            ),
        ];
        for i in 0..pages {
            objects.push((i + 5, "<</Type/Page/Parent 2 0 R>>".to_string()));
        }
        let (payload, first) = objstm_payload(&objects);

        let mut b = Builder::new();
        b.stream(
            4,
            &format!(
                "/Type/ObjStm/N {}/First {first}/Filter/FlateDecode",
                objects.len()
            ),
            &deflate(&payload),
        );

        let last = 4 + pages;
        let xref_at = b.len();
        let mut entries = vec![
            (0u8, 0usize, 0xffffu16),
            // The catalog and the page tree: type 2, inside object stream 4.
            (2, 4, 0),
            (2, 4, 1),
            // The cross-reference stream itself, then the object stream.
            (1, xref_at, 0),
            (1, b.offset(4), 0),
        ];
        for n in 5..=last {
            // The page objects follow the catalog and the tree in the stream.
            entries.push((2, 4, u16::try_from(n - 3).expect("few pages")));
        }
        let rows = xref_rows(&entries);
        let size = last + 1;
        let (data, extra) = if predictor {
            (
                png_up(&rows, 7),
                format!("/Index[0 {size}]/DecodeParms<</Predictor 12/Columns 7>>"),
            )
        } else {
            (rows, String::new())
        };
        b.stream(
            3,
            &format!("/Type/XRef/Size {size}/W[1 4 2]/Root 1 0 R/Filter/FlateDecode{extra}"),
            &deflate(&data),
        );
        b.finish(xref_at)
    }

    /// The same shape, but the object stream inflates to `inflated` bytes — a
    /// small file that asks the parser to allocate a large one.
    pub(crate) fn objstm_bomb(inflated: usize) -> Vec<u8> {
        let (mut payload, first) = objstm_payload(&[
            (1, CATALOG.to_string()),
            (2, "<</Type/Pages/Kids[5 0 R]/Count 1>>".to_string()),
        ]);
        payload.resize(inflated, b' ');

        let mut b = Builder::new();
        b.stream(
            4,
            &format!("/Type/ObjStm/N 2/First {first}/Filter/FlateDecode"),
            &deflate(&payload),
        );
        b.obj(5, "<</Type/Page/Parent 2 0 R>>");
        let xref_at = b.len();
        let rows = xref_rows(&[
            (0, 0, 0xffff),
            (2, 4, 0),
            (2, 4, 1),
            (1, xref_at, 0),
            (1, b.offset(4), 0),
            (1, b.offset(5), 0),
        ]);
        b.stream(
            3,
            "/Type/XRef/Size 6/W[1 4 2]/Root 1 0 R/Filter/FlateDecode",
            &deflate(&rows),
        );
        b.finish(xref_at)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{
        Builder, classic, classic_with_count, deflate, encrypted, incremental, objstm_bomb,
        objstm_payload, page_text_in_content, xref_rows, xref_stream_objstm,
    };
    use super::*;

    /// The classic cross-reference table with an uncompressed page tree: the
    /// count comes off the tree's root node.
    #[test]
    fn a_classic_table_is_read_from_its_page_tree() {
        assert_eq!(page_count(&classic(1)), Some(1));
        assert_eq!(page_count(&classic(3)), Some(3));
        assert_eq!(page_count(&classic(41)), Some(41));
    }

    /// The case a byte scan cannot see, and the whole reason this module
    /// exists: a cross-reference *stream* whose catalog and page tree live
    /// inside a compressed object stream.
    #[test]
    fn a_compressed_page_tree_is_read_not_guessed() {
        for predictor in [false, true] {
            let doc = xref_stream_objstm(7, predictor);
            for invisible in [b"/Count".as_slice(), b"/Type/Page", b"/Type/Catalog"] {
                assert!(
                    find(&doc, invisible).is_none(),
                    "the page tree must be invisible to a byte scan (predictor {predictor})"
                );
            }
            assert_eq!(page_count(&doc), Some(7), "predictor {predictor}");
        }
    }

    /// `/Prev` chains (§7.5.6): the newest revision wins, and the two
    /// revisions here declare different counts so that reading the wrong one
    /// cannot pass.
    #[test]
    fn an_incremental_update_beats_the_revision_it_replaced() {
        let (base, updated) = incremental();
        assert_eq!(page_count(&base), Some(3));
        assert_eq!(page_count(&updated), Some(5));
        // The older revision is still in the file, and still reachable — an
        // update only appends — so answering 5 is a statement about
        // precedence, not about which bytes exist.
        assert!(find(&updated, b"/Count 3").is_some());
    }

    /// `/Type /Page` inside a string and inside a content stream: syntax, not
    /// page objects.
    #[test]
    fn page_text_in_a_string_is_not_a_page() {
        let doc = page_text_in_content();
        assert!(
            doc.windows(11)
                .filter(|w| *w == b"/Type /Page".as_slice())
                .count()
                > 3,
            "the fixture must contain more of that text than the file has pages"
        );
        assert_eq!(page_count(&doc), Some(3));
    }

    /// The PNG row filters of RFC 2083 §6, each undone against values worked
    /// out by hand. Every filter yields a different row, so a slip in one arm
    /// cannot hide behind another.
    #[test]
    fn png_predictors_are_undone_row_by_row() {
        let mut data = Vec::new();
        for (filter, row) in [
            (1u8, [10u8, 5, 5, 5]), // Sub
            (2, [1, 1, 1, 1]),      // Up
            (0, [7, 7, 7, 7]),      // None
            (3, [1, 2, 3, 4]),      // Average
            (4, [1, 1, 1, 1]),      // Paeth
        ] {
            data.push(filter);
            data.extend_from_slice(&row);
        }
        assert_eq!(
            undo_png_predictor(&data, 1, 8, 4),
            Some(vec![
                10, 15, 20, 25, // Sub: plus the byte one bpp to the left
                11, 16, 21, 26, // Up: plus the byte above
                7, 7, 7, 7, // None
                4, 7, 10, 12, // Average: plus ⌊(left + above) / 2⌋
                5, 8, 11, 13, // Paeth
            ])
        );
        // Parameters that do not divide the data are parameters that were
        // misread, and a misread predictor yields garbage, not an error.
        assert_eq!(undo_png_predictor(&data, 1, 8, 3), None);
        assert_eq!(undo_png_predictor(&data, 0, 8, 4), None);
        assert_eq!(undo_png_predictor(&data, 1, 7, 4), None);
        assert_eq!(undo_png_predictor(b"", 1, 8, 4), None);
        // `/Colors` is file-controlled, so its UPPER bound is what stops
        // `colors * bpc * columns` overflowing — and the bound has to be
        // asserted here, because a nonsense `colors` small enough to multiply
        // safely is refused further down by the row-length check instead (which
        // is why the `0` above proves nothing about the range test).
        assert_eq!(undo_png_predictor(&data, 33, 8, 4), None);
        assert_eq!(undo_png_predictor(&data, usize::MAX / 4, 8, 1), None);
    }

    /// A `/Prev` pointing at its own section, and two sections pointing at
    /// each other. The assertion that matters as much as the value is that
    /// this test finishes at all.
    #[test]
    fn a_prev_loop_terminates() {
        let mut b = Builder::new();
        b.obj(1, "<</Type/Catalog/Pages 2 0 R>>");
        b.obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>");
        b.obj(3, "<</Type/Page/Parent 2 0 R>>");
        let at = b.len();
        let doc = {
            let table = b.xref_table(&format!("/Prev {at}"));
            assert_eq!(table, at, "the section names its own offset");
            b.finish(table)
        };
        assert_eq!(page_count(&doc), None);

        // Two sections naming each other. The offsets are written at a fixed
        // width so that the first can name the second before it exists.
        let mut doc = b"%PDF-1.7\n1 0 obj\n<</Type/Catalog/Pages 2 0 R>>\nendobj\n".to_vec();
        let section = |prev: usize| {
            format!("xref\n0 1\n0000000000 65535 f \ntrailer\n<</Root 1 0 R/Prev {prev:010}>>\n")
        };
        let first = doc.len();
        let second = first + section(0).len();
        doc.extend_from_slice(section(second).as_bytes());
        assert_eq!(doc.len(), second, "the two sections are the same length");
        doc.extend_from_slice(section(first).as_bytes());
        doc.extend_from_slice(format!("startxref\n{second}\n%%EOF\n").as_bytes());
        assert_eq!(page_count(&doc), None);
    }

    /// Offsets that do not name what they claim to.
    #[test]
    fn offsets_outside_the_file_are_refused() {
        let doc = classic(3);
        let repoint = |offset: usize| {
            let mut d = doc.clone();
            d.truncate(rfind(&doc, b"startxref").expect("the fixture has one"));
            d.extend_from_slice(format!("startxref\n{offset}\n%%EOF\n").as_bytes());
            d
        };
        // Past EOF, exactly at EOF, and inside the file but not at a section.
        assert_eq!(page_count(&repoint(doc.len() * 4)), None);
        assert_eq!(page_count(&repoint(doc.len())), None);
        assert_eq!(page_count(&repoint(3)), None);
    }

    /// Truncation, at every length. Each prefix is a file some producer could
    /// have flushed before dying: none may panic, hang, or invent a count the
    /// document never declared.
    #[test]
    fn every_truncation_is_refused_without_panicking() {
        for doc in [classic(3), xref_stream_objstm(3, true)] {
            for cut in 0..doc.len() {
                let prefix = doc.get(..cut).expect("the cut is inside the document");
                let pages = page_count(prefix);
                assert!(
                    pages.is_none_or(|n| n == 3),
                    "a file cut at {cut} answered {pages:?}"
                );
            }
            assert_eq!(page_count(&doc), Some(3), "the uncut file still reads");
        }
    }

    /// A decompression bomb: an object stream of a few kilobytes that names
    /// more output than [`MAX_INFLATED_BYTES`] allows.
    #[test]
    fn an_inflate_past_the_ceiling_is_refused() {
        let bomb = objstm_bomb(MAX_INFLATED_BYTES + 1);
        assert!(
            bomb.len() * 32 < MAX_INFLATED_BYTES,
            "the file is far smaller than what it asks to allocate: {} bytes",
            bomb.len()
        );
        assert_eq!(page_count(&bomb), None);
        // The ceiling is a ceiling, not a floor: a stream inflating to exactly
        // it is still read.
        assert_eq!(page_count(&objstm_bomb(MAX_INFLATED_BYTES)), Some(1));
    }

    /// Counts a document has no business declaring.
    #[test]
    fn an_unbelievable_count_is_no_count_at_all() {
        assert_eq!(page_count(&classic_with_count("1")), Some(1));
        for count in [
            "-1",
            "0",
            "999999999",
            "99999999999999999999",
            "/Nope",
            "(3)",
            "3.5",
            "null",
            "true",
        ] {
            assert_eq!(
                page_count(&classic_with_count(count)),
                None,
                "/Count {count} is not a page count"
            );
        }

        // `/Count` as an indirect reference is legal, and is resolved.
        let mut b = Builder::new();
        b.obj(1, "<</Type/Catalog/Pages 2 0 R>>");
        b.obj(2, "<</Type/Pages/Kids[3 0 R]/Count 4 0 R>>");
        b.obj(3, "<</Type/Page/Parent 2 0 R>>");
        b.obj(4, "2");
        let at = b.xref_table("");
        let doc = b.finish(at);
        assert_eq!(page_count(&doc), Some(2));
    }

    /// An encrypted file: detected, never decrypted, never guessed at.
    #[test]
    fn an_encrypted_document_falls_back() {
        assert_eq!(page_count(&encrypted()), None);
    }

    /// The inputs that are not documents at all.
    #[test]
    fn junk_is_refused() {
        assert_eq!(page_count(b""), None);
        assert_eq!(page_count(b"%PDF-1.7\n"), None);
        assert_eq!(page_count(b"not a pdf, not even close"), None);
        assert_eq!(page_count(&vec![0u8; 4096]), None);
        assert_eq!(page_count(&vec![b'('; 100_000]), None);
        assert_eq!(page_count(&vec![b'['; 100_000]), None);

        // A trailer naming a catalog the file does not contain.
        let mut b = Builder::new();
        b.obj(1, "<</Type/Catalog/Pages 9 0 R>>");
        let at = b.xref_table("");
        let doc = b.finish(at);
        assert_eq!(page_count(&doc), None);
    }

    /// Single-byte corruptions of real PDFs, walked deterministically. The
    /// point is not the answers — it is that there is always an answer: no
    /// panic, no hang, and never a count outside what a document could
    /// declare.
    #[test]
    fn corrupted_bytes_never_panic() {
        let mut seed = 0x5eed_1234_u64;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) as usize
        };
        for base in [
            classic(3),
            xref_stream_objstm(3, true),
            page_text_in_content(),
        ] {
            for _ in 0..2_000 {
                let mut doc = base.clone();
                let at = next() % doc.len();
                *doc.get_mut(at).expect("the index is inside the document") = (next() % 256) as u8;
                let pages = page_count(&doc);
                assert!(
                    pages.is_none_or(|n| (1..=MAX_PAGE_COUNT).contains(&n)),
                    "byte {at} corrupted into a count of {pages:?}"
                );
            }
        }
    }

    /// A stream whose `/Length` is an indirect reference, one whose `/Length`
    /// is simply wrong, and one whose `/Length` runs off the end of the file —
    /// all common enough in the wild that the parser finds the end of the data
    /// for itself. The last is also the one that turns an unchecked slice into
    /// a panic.
    #[test]
    fn a_wrong_length_falls_back_to_endstream() {
        let (payload, first) = objstm_payload(&[
            (1, fixtures::CATALOG.to_string()),
            (2, "<</Type/Pages/Kids[5 0 R]/Count 6>>".to_string()),
        ]);
        let data = deflate(&payload);
        for length in [
            "/Length 7 0 R".to_string(),
            format!("/Length {}", data.len() + 99),
            "/Length 10000000".to_string(),
        ] {
            let mut b = Builder::new();
            b.stream_verbatim(
                4,
                &format!("/Type/ObjStm/N 2/First {first}/Filter/FlateDecode{length}"),
                &data,
            );
            b.obj(5, "<</Type/Page/Parent 2 0 R>>");
            let xref_at = b.len();
            let rows = xref_rows(&[
                (0, 0, 0xffff),
                (2, 4, 0),
                (2, 4, 1),
                (1, xref_at, 0),
                (1, b.offset(4), 0),
                (1, b.offset(5), 0),
            ]);
            b.stream(
                3,
                "/Type/XRef/Size 6/W[1 4 2]/Root 1 0 R/Filter/FlateDecode",
                &deflate(&rows),
            );
            let doc = b.finish(xref_at);
            assert_eq!(page_count(&doc), Some(6), "with {length}");
        }
    }

    /// The bounds are this parser's only defence against a file built to waste
    /// time, so each is exercised where it is cheap to reach.
    #[test]
    fn the_bounds_hold() {
        // A `/Prev` chain one section longer than MAX_XREF_SECTIONS.
        let mut doc = classic(3);
        let mut previous = {
            let at = rfind(&doc, b"startxref").expect("the fixture has one");
            let mut lx = Lexer::at(&doc, at + b"startxref".len()).expect("inside the file");
            usize::try_from(lx.integer().expect("an offset")).expect("small")
        };
        for _ in 0..MAX_XREF_SECTIONS {
            let at = doc.len();
            doc.extend_from_slice(
                format!(
                    "xref\n0 1\n0000000000 65535 f \ntrailer\n<</Root 1 0 R/Prev {previous}>>\n\
                     startxref\n{at}\n%%EOF\n"
                )
                .as_bytes(),
            );
            previous = at;
        }
        assert_eq!(page_count(&doc), None, "a chain past the section bound");

        // Nesting past MAX_NESTING_DEPTH.
        let deep = format!(
            "{}3{}",
            "[".repeat(MAX_NESTING_DEPTH + 2),
            "]".repeat(MAX_NESTING_DEPTH + 2)
        );
        let mut b = Builder::new();
        b.obj(1, "<</Type/Catalog/Pages 2 0 R>>");
        b.obj(2, &format!("<</Type/Pages/Kids{deep}/Count 1>>"));
        b.obj(3, "<</Type/Page/Parent 2 0 R>>");
        let at = b.xref_table("");
        let doc = b.finish(at);
        assert_eq!(page_count(&doc), None, "nesting past the depth bound");

        // A name past MAX_NAME_BYTES, as a key with a value of its own so that
        // the dictionary around it is otherwise well formed — the name's
        // length has to be the only thing wrong with it.
        let mut b = Builder::new();
        b.obj(1, "<</Type/Catalog/Pages 2 0 R>>");
        b.obj(
            2,
            &format!(
                "<</Type/Pages/{} 0/Kids[3 0 R]/Count 1>>",
                "N".repeat(MAX_NAME_BYTES + 1)
            ),
        );
        b.obj(3, "<</Type/Page/Parent 2 0 R>>");
        let at = b.xref_table("");
        let doc = b.finish(at);
        assert_eq!(page_count(&doc), None, "a name past the length bound");
    }
}
