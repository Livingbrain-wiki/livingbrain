//! Names and avatars: what a member calls themselves, what owners and admins
//! call the workspace, and the small square image beside each.
//!
//! ## Where a profile lives
//!
//! On the member row. A member id is minted per workspace (ADR 0002) and
//! there is no person row above the workspaces to hang a profile on — the
//! only thing that joins one person's memberships is an email identity, and
//! a Slack-only member has none. So a name and an avatar are per membership:
//! the person who is "Ada" at work can be "Ada L." in the side project, and
//! nothing is shared across tenants that a tenant did not write itself.
//!
//! `display_name` is a column of its own rather than a write to `name`,
//! because `name` is the mirror of what the sign-in method reports and
//! Slack's `user_change` rewrites it. The name everybody reads is the chosen
//! one when there is one, the mirrored one otherwise — every member read
//! selects it under the column name `name`, so readers that already show
//! `name` show the chosen one without a change.
//!
//! ## Avatars
//!
//! An avatar is an opaque id (`av_<ulid>`) on the row and an image in the
//! blob store at `avatars/<workspace id>/<avatar id>`. Each upload mints a
//! new id, so a URL never changes meaning and can be cached as immutable;
//! the previous image is deleted once the row points at the new one.
//!
//! What is stored is never what was sent: [`clean_image`] accepts only PNG,
//! JPEG and WebP **by their magic bytes**, refuses one larger than
//! [`AVATAR_MAX_BYTES`] or wider or taller than [`AVATAR_MAX_SIDE`], and
//! rewrites the container without its metadata — EXIF (with its GPS),
//! XMP, ICC profiles, comments, text chunks — and without animation frames
//! a PNG carries, so a photo taken on a phone does not publish where it was
//! taken. SVG is refused outright: it is a document, not an image.
//!
//! ## The key space
//!
//! Three modules touch avatars: `workspaces` (the workspace's), `me` (a
//! member's own) and `avatars` (the read route). The harness scopes each
//! module's blob view under its own name, so the composition plants an
//! [`AvatarRebind`] under those views: `me/avatars/…`, `workspaces/avatars/…`
//! and `avatars/avatars/…` all land on the one physical `avatars/…` space,
//! and every other key passes through untouched.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cratefield_core::axum::http::StatusCode;
use cratefield_core::{
    Blob, BlobError, BlobObject, Database, DbError, PresignedPut, Problem, ProblemDef, Statement,
};
use sea_query::Value;

/// The largest image accepted, in bytes, before cleaning: 512 KiB.
pub const AVATAR_MAX_BYTES: usize = 512 * 1024;

/// The transport ceiling for an upload body: the image limit plus room for
/// the handler to answer a precise `413` itself.
pub const AVATAR_BODY_CEILING: usize = AVATAR_MAX_BYTES + 64 * 1024;

/// The widest and tallest image accepted, in pixels. A 512 KiB PNG can still
/// declare a canvas that takes a browser gigabytes to decode; this caps it.
pub const AVATAR_MAX_SIDE: u32 = 4096;

/// How long a browser may keep an avatar: a year, privately. The id changes
/// on every upload, so a cached copy can never be stale under a live id.
pub const AVATAR_CACHE_CONTROL: &str = "private, max-age=31536000, immutable";

/// The longest name, in characters, after trimming.
pub const NAME_MAX_CHARS: usize = 64;

/// The first segment of every avatar key, and of the rebind's target.
pub const AVATAR_PREFIX: &str = "avatars";

// ---------------------------------------------------------------------------
// Names

/// Why a name was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    /// Nothing but whitespace.
    Empty,
    /// More than [`NAME_MAX_CHARS`] characters after trimming.
    TooLong,
    /// A control character, or an invisible one that changes how the text
    /// around it reads (a bidi override, a zero-width joiner).
    Invisible,
}

impl NameError {
    /// One sentence a person can act on.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Empty => "A name needs at least one visible character.",
            Self::TooLong => "A name is at most 64 characters.",
            Self::Invisible => {
                "A name cannot hold control characters or invisible formatting characters."
            }
        }
    }
}

/// Characters that are not control characters but still change how a name
/// reads without being seen: zero-width ones, bidi embeddings, overrides and
/// isolates, and the byte-order mark. A name holding one can impersonate
/// another in a list ("Ada" vs "Ada\u{200B}"), or reverse the text after it.
fn invisible(ch: char) -> bool {
    matches!(
        ch,
        '\u{00AD}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// A name as it is stored: trimmed, 1–64 characters, no control or
/// invisible formatting characters.
///
/// # Errors
///
/// [`NameError`] naming the first rule the input breaks.
pub fn clean_name(raw: &str) -> Result<String, NameError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(NameError::Empty);
    }
    if trimmed.chars().any(|ch| ch.is_control() || invisible(ch)) {
        return Err(NameError::Invisible);
    }
    if trimmed.chars().count() > NAME_MAX_CHARS {
        return Err(NameError::TooLong);
    }
    Ok(trimmed.to_owned())
}

// ---------------------------------------------------------------------------
// Images

/// The three image types an avatar may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Webp,
}

impl ImageKind {
    /// The media type the image is served with.
    #[must_use]
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }

    /// The file extension for `Content-Disposition`.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
        }
    }

    /// The kind a declared `Content-Type` names, parameters ignored, or
    /// `None` for anything else (`image/svg+xml` and `image/gif` included).
    #[must_use]
    pub fn from_mime(value: &str) -> Option<Self> {
        let essence = value.split(';').next().unwrap_or_default().trim();
        match essence.to_ascii_lowercase().as_str() {
            "image/png" => Some(Self::Png),
            "image/jpeg" | "image/jpg" => Some(Self::Jpeg),
            "image/webp" => Some(Self::Webp),
            _ => None,
        }
    }

    /// The kind the bytes **are**, by their magic number, or `None`.
    #[must_use]
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(PNG_SIGNATURE) {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Self::Jpeg)
        } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Some(Self::Webp)
        } else {
            None
        }
    }
}

/// Why an image was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageError {
    /// Larger than [`AVATAR_MAX_BYTES`].
    TooLarge,
    /// Not PNG, JPEG or WebP by its bytes, or a `Content-Type` that is not
    /// one of those three.
    Unsupported,
    /// The declared type and the bytes disagree.
    Mismatch,
    /// The bytes start like an image and then stop making sense.
    Malformed,
    /// Wider or taller than [`AVATAR_MAX_SIDE`], or zero pixels.
    Dimensions,
}

impl ImageError {
    /// One sentence a person can act on.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::TooLarge => "An avatar is at most 512 KB.",
            Self::Unsupported => "An avatar is a PNG, JPEG or WebP image.",
            Self::Mismatch => "The file's contents are not the image type it was sent as.",
            Self::Malformed => "The image could not be read.",
            Self::Dimensions => "An avatar is at most 4096 pixels on each side.",
        }
    }
}

/// An image that passed [`clean_image`]: its kind, its size, and the bytes to
/// store — the input without its metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanImage {
    pub kind: ImageKind,
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

/// Checks an upload and rewrites it without metadata.
///
/// `declared` is the request's `Content-Type`; when present it must name one
/// of the three types **and** agree with the bytes. The bytes decide either
/// way: a PNG sent as `image/jpeg` is refused, and so is an SVG sent as
/// `image/png`.
///
/// # Errors
///
/// [`ImageError`] naming the first check the upload fails.
pub fn clean_image(bytes: &[u8], declared: Option<&str>) -> Result<CleanImage, ImageError> {
    if bytes.len() > AVATAR_MAX_BYTES {
        return Err(ImageError::TooLarge);
    }
    let claimed = match declared {
        Some(value) => Some(ImageKind::from_mime(value).ok_or(ImageError::Unsupported)?),
        None => None,
    };
    let kind = ImageKind::sniff(bytes).ok_or(ImageError::Unsupported)?;
    if claimed.is_some_and(|claimed| claimed != kind) {
        return Err(ImageError::Mismatch);
    }
    let (width, height, bytes) = match kind {
        ImageKind::Png => clean_png(bytes)?,
        ImageKind::Jpeg => clean_jpeg(bytes)?,
        ImageKind::Webp => clean_webp(bytes)?,
    };
    if width == 0 || height == 0 || width > AVATAR_MAX_SIDE || height > AVATAR_MAX_SIDE {
        return Err(ImageError::Dimensions);
    }
    Ok(CleanImage {
        kind,
        width,
        height,
        bytes,
    })
}

const PNG_SIGNATURE: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

fn be32(bytes: &[u8], at: usize) -> Result<u32, ImageError> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(ImageError::Malformed)
}

fn le32(bytes: &[u8], at: usize) -> Result<u32, ImageError> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(ImageError::Malformed)
}

fn le24(bytes: &[u8], at: usize) -> Result<u32, ImageError> {
    bytes
        .get(at..at + 3)
        .map(|b| u32::from(b[0]) | (u32::from(b[1]) << 8) | (u32::from(b[2]) << 16))
        .ok_or(ImageError::Malformed)
}

fn usize_of(value: u32) -> Result<usize, ImageError> {
    usize::try_from(value).map_err(|_| ImageError::Malformed)
}

/// PNG: the chunks a still image needs to decode and to look right, and
/// nothing else. Text (`tEXt`, `zTXt`, `iTXt`), `eXIf`, `tIME`, the ICC
/// profile and every private chunk are dropped; so are the APNG frames, which
/// leaves the default image. Each chunk carries its own CRC, so dropping
/// whole chunks keeps the file valid.
fn clean_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), ImageError> {
    const KEEP: [&[u8; 4]; 11] = [
        b"IHDR", b"PLTE", b"IDAT", b"IEND", b"tRNS", b"gAMA", b"cHRM", b"sRGB", b"sBIT", b"pHYs",
        b"bKGD",
    ];
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(PNG_SIGNATURE);
    let mut at = PNG_SIGNATURE.len();
    let mut size = None;
    let mut saw_idat = false;
    loop {
        let length = usize_of(be32(bytes, at)?)?;
        let kind = bytes.get(at + 4..at + 8).ok_or(ImageError::Malformed)?;
        let end = at
            .checked_add(12)
            .and_then(|n| n.checked_add(length))
            .ok_or(ImageError::Malformed)?;
        let chunk = bytes.get(at..end).ok_or(ImageError::Malformed)?;
        if size.is_none() {
            // IHDR comes first and is 13 bytes: width, height, then flags.
            if kind != b"IHDR" || length != 13 {
                return Err(ImageError::Malformed);
            }
            size = Some((be32(bytes, at + 8)?, be32(bytes, at + 12)?));
        }
        if kind == b"IDAT" {
            saw_idat = true;
        }
        if KEEP.iter().any(|keep| keep.as_slice() == kind) {
            out.extend_from_slice(chunk);
        }
        at = end;
        if kind == b"IEND" {
            break;
        }
    }
    let (width, height) = size.ok_or(ImageError::Malformed)?;
    if !saw_idat {
        return Err(ImageError::Malformed);
    }
    Ok((width, height, out))
}

/// JPEG: every header segment except the application segments that carry
/// metadata and the comments. `APP0` (JFIF) and `APP14` (Adobe, which says
/// how to read the colour channels) stay; `APP1` (EXIF and XMP), `APP2` (ICC
/// and FlashPix), `APP13` (IPTC) and the rest go, as does `COM`. From the
/// first start-of-scan on the bytes are image data and are copied up to and
/// including the last end-of-image marker; anything after it is dropped.
fn clean_jpeg(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), ImageError> {
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(&[0xFF, 0xD8]);
    let mut at = 2;
    let mut size = None;
    loop {
        // Markers may be padded with any number of 0xFF fill bytes.
        if bytes.get(at) != Some(&0xFF) {
            return Err(ImageError::Malformed);
        }
        while bytes.get(at) == Some(&0xFF) {
            at += 1;
        }
        let marker = *bytes.get(at).ok_or(ImageError::Malformed)?;
        at += 1;
        match marker {
            // Standalone markers carry no length.
            0x01 | 0xD0..=0xD7 => {
                out.extend_from_slice(&[0xFF, marker]);
                continue;
            }
            // An end-of-image before any scan has no picture in it.
            0xD8 | 0xD9 => return Err(ImageError::Malformed),
            _ => {}
        }
        let length = usize::from(u16::from_be_bytes([
            *bytes.get(at).ok_or(ImageError::Malformed)?,
            *bytes.get(at + 1).ok_or(ImageError::Malformed)?,
        ]));
        if length < 2 {
            return Err(ImageError::Malformed);
        }
        let segment = bytes.get(at..at + length).ok_or(ImageError::Malformed)?;
        // Start-of-frame (every SOFn but DHT, JPG and DAC): precision, then
        // height, then width.
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            if segment.len() < 7 {
                return Err(ImageError::Malformed);
            }
            let height = u32::from(u16::from_be_bytes([segment[3], segment[4]]));
            let width = u32::from(u16::from_be_bytes([segment[5], segment[6]]));
            size = Some((width, height));
        }
        let metadata = matches!(marker, 0xE1..=0xED | 0xEF | 0xFE);
        if !metadata {
            out.extend_from_slice(&[0xFF, marker]);
            out.extend_from_slice(segment);
        }
        at += length;
        if marker == 0xDA {
            // Start of scan: the rest is entropy-coded data, up to the last
            // end-of-image marker.
            let rest = bytes.get(at..).ok_or(ImageError::Malformed)?;
            let end = rest
                .windows(2)
                .rposition(|pair| pair == [0xFF, 0xD9])
                .ok_or(ImageError::Malformed)?;
            out.extend_from_slice(&rest[..end + 2]);
            break;
        }
    }
    let (width, height) = size.ok_or(ImageError::Malformed)?;
    Ok((width, height, out))
}

/// WebP: the RIFF container without its `EXIF`, `XMP ` and `ICCP` chunks,
/// with the extended header's flags for them cleared and the RIFF length
/// rewritten. The image chunks (`VP8 `, `VP8L`, `ALPH`, and an animation's
/// `ANIM`/`ANMF`) are copied as they are.
fn clean_webp(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), ImageError> {
    const DROP: [&[u8; 4]; 3] = [b"EXIF", b"XMP ", b"ICCP"];
    let declared = usize_of(le32(bytes, 4)?)?;
    let end = declared.checked_add(8).ok_or(ImageError::Malformed)?;
    let body = bytes.get(12..end).ok_or(ImageError::Malformed)?;
    let mut chunks = Vec::with_capacity(body.len());
    let mut size = None;
    let mut at = 0;
    while at < body.len() {
        let kind = body.get(at..at + 4).ok_or(ImageError::Malformed)?;
        let length = usize_of(le32(body, at + 4)?)?;
        let padded = length + (length & 1);
        let data_end = at + 8 + length;
        let data = body.get(at + 8..data_end).ok_or(ImageError::Malformed)?;
        let chunk_end = (at + 8 + padded).min(body.len());
        match kind {
            b"VP8X" => {
                // Flags, three reserved bytes, then the canvas size minus one.
                if data.len() < 10 {
                    return Err(ImageError::Malformed);
                }
                size = Some((le24(data, 4)? + 1, le24(data, 7)? + 1));
                let mut header = body[at..chunk_end].to_vec();
                // ICC (0x20), EXIF (0x08) and XMP (0x04) are no longer present.
                header[8] &= !(0x20 | 0x08 | 0x04);
                chunks.extend_from_slice(&header);
            }
            b"VP8 " => {
                // A frame tag, the start code 9d 01 2a, then 14-bit sizes.
                if data.len() < 10 || data[3..6] != [0x9D, 0x01, 0x2A] {
                    return Err(ImageError::Malformed);
                }
                let width = u32::from(u16::from_le_bytes([data[6], data[7]]) & 0x3FFF);
                let height = u32::from(u16::from_le_bytes([data[8], data[9]]) & 0x3FFF);
                size.get_or_insert((width, height));
                chunks.extend_from_slice(&body[at..chunk_end]);
            }
            b"VP8L" => {
                // The signature 0x2f, then width-1 and height-1 in 14 bits each.
                if data.len() < 5 || data[0] != 0x2F {
                    return Err(ImageError::Malformed);
                }
                let bits = le32(data, 1)?;
                let width = (bits & 0x3FFF) + 1;
                let height = ((bits >> 14) & 0x3FFF) + 1;
                size.get_or_insert((width, height));
                chunks.extend_from_slice(&body[at..chunk_end]);
            }
            other if DROP.iter().any(|drop| drop.as_slice() == other) => {}
            _ => chunks.extend_from_slice(&body[at..chunk_end]),
        }
        at = chunk_end;
    }
    let (width, height) = size.ok_or(ImageError::Malformed)?;
    let riff_len = u32::try_from(chunks.len() + 4).map_err(|_| ImageError::Malformed)?;
    let mut out = Vec::with_capacity(chunks.len() + 12);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_len.to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend_from_slice(&chunks);
    Ok((width, height, out))
}

// ---------------------------------------------------------------------------
// Refusals, shared by every route that writes a name or an avatar

const INVALID_NAME: ProblemDef = ProblemDef {
    slug: "profiles/invalid-name",
    status: StatusCode::UNPROCESSABLE_ENTITY,
    title: "That name cannot be used",
    description: "A name is 1 to 64 visible characters.",
};

const NOTHING_TO_CHANGE: ProblemDef = ProblemDef {
    slug: "profiles/nothing-to-change",
    status: StatusCode::UNPROCESSABLE_ENTITY,
    title: "Nothing to change",
    description: "Send the field to change.",
};

const IMAGE_TOO_LARGE: ProblemDef = ProblemDef {
    slug: "profiles/image-too-large",
    status: StatusCode::PAYLOAD_TOO_LARGE,
    title: "The image is too large",
    description: "An avatar is at most 512 KB.",
};

const UNSUPPORTED_IMAGE: ProblemDef = ProblemDef {
    slug: "profiles/unsupported-image",
    status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
    title: "That is not a PNG, JPEG or WebP image",
    description: "An avatar is a PNG, JPEG or WebP image, sent with its own Content-Type.",
};

const INVALID_IMAGE: ProblemDef = ProblemDef {
    slug: "profiles/invalid-image",
    status: StatusCode::UNPROCESSABLE_ENTITY,
    title: "The image cannot be used",
    description: "It could not be read, or it is larger than 4096 pixels on a side.",
};

const AVATARS_UNAVAILABLE: ProblemDef = ProblemDef {
    slug: "profiles/avatars-unavailable",
    status: StatusCode::SERVICE_UNAVAILABLE,
    title: "Avatars are not available here",
    description: "This deployment has no blob store wired.",
};

/// A refused name, as a `422` naming the rule.
#[must_use]
pub fn name_problem(error: NameError) -> Problem {
    Problem::new(&INVALID_NAME).with_detail(error.message())
}

/// A `PATCH` with no field this route changes.
#[must_use]
pub fn nothing_to_change() -> Problem {
    Problem::new(&NOTHING_TO_CHANGE)
}

/// A refused image: `413` too large, `415` not one of the three types (or
/// not the type it was sent as), `422` unreadable or too many pixels.
#[must_use]
pub fn image_problem(error: ImageError) -> Problem {
    let def = match error {
        ImageError::TooLarge => &IMAGE_TOO_LARGE,
        ImageError::Unsupported | ImageError::Mismatch => &UNSUPPORTED_IMAGE,
        ImageError::Malformed | ImageError::Dimensions => &INVALID_IMAGE,
    };
    Problem::new(def).with_detail(error.message())
}

/// No blob store: a `503`, as the module's other unwired ports answer.
#[must_use]
pub fn avatars_unavailable() -> Problem {
    Problem::new(&AVATARS_UNAVAILABLE)
}

/// A blob store failure: a 500 with nothing of the store's message in it.
#[must_use]
pub fn blob_failed(error: &BlobError) -> Problem {
    let kind = match error {
        BlobError::Operation(_) => "operation",
        BlobError::BadKey(_) => "bad-key",
        BlobError::TooLarge(_) => "too-large",
        BlobError::Unsupported(_) => "unsupported",
    };
    eprintln!("an avatar blob operation failed: {kind}");
    Problem::internal()
}

/// Stores a cleaned image under a fresh id in `workspace_id`'s avatar space
/// and answers the id.
///
/// # Errors
///
/// A 500 [`Problem`] when the store refuses it.
pub async fn store_avatar(
    blob: &dyn Blob,
    workspace_id: &str,
    ulid: &str,
    image: &CleanImage,
) -> Result<String, Problem> {
    let id = new_avatar_id(ulid);
    blob.put(
        &avatar_key(workspace_id, &id),
        &image.bytes,
        image.kind.mime(),
    )
    .await
    .map_err(|error| blob_failed(&error))?;
    Ok(id)
}

/// Deletes the image a row named before it was replaced or cleared. Best
/// effort: the row no longer names it, so the read route already refuses
/// it, and a failed delete leaves an object nobody can reach.
pub async fn forget_avatar(
    blob: &dyn Blob,
    workspace_id: &str,
    previous: Option<&str>,
    current: Option<&str>,
) {
    if let Some(old) = previous.filter(|old| is_avatar_id(old) && Some(*old) != current) {
        let _ = blob.delete(&avatar_key(workspace_id, old)).await;
    }
}

// ---------------------------------------------------------------------------
// Avatar ids, keys and URLs

/// An avatar id as the rows and URLs carry it: `av_` and a 26-character ULID.
#[must_use]
pub fn is_avatar_id(value: &str) -> bool {
    value.strip_prefix("av_").is_some_and(|ulid| {
        ulid.len() == 26
            && ulid
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
    })
}

/// A fresh avatar id from a ULID.
#[must_use]
pub fn new_avatar_id(ulid: &str) -> String {
    format!("av_{ulid}")
}

/// The key an avatar is stored under, as a module names it inside its own
/// scoped view; [`AvatarRebind`] lands it on `avatars/<workspace>/<id>`.
#[must_use]
pub fn avatar_key(workspace_id: &str, avatar_id: &str) -> String {
    format!("{AVATAR_PREFIX}/{workspace_id}/{avatar_id}")
}

/// The same-origin URL an avatar is read from.
#[must_use]
pub fn avatar_url(avatar_id: &str) -> String {
    format!("/v1/avatars/{avatar_id}")
}

/// An avatar a row names, for a view: its id and the URL to read it from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AvatarRef {
    pub id: String,
    pub url: String,
}

impl AvatarRef {
    /// The reference for a stored id, or `None` for no avatar (or a value
    /// that is not an avatar id, which no route writes).
    #[must_use]
    pub fn of(stored: Option<&str>) -> Option<Self> {
        stored.filter(|id| is_avatar_id(id)).map(|id| Self {
            id: id.to_owned(),
            url: avatar_url(id),
        })
    }
}

/// The modules whose scoped avatar keys the rebind lands on the one space.
const AVATAR_MODULES: [&str; 3] = ["me", "workspaces", "avatars"];

/// Lands every avatar key on one key space.
///
/// The harness scopes a module's blob view under its own name, so the same
/// [`avatar_key`] reaches the store as `me/avatars/…` from `PUT /v1/me/avatar`,
/// `workspaces/avatars/…` from the workspace's upload and
/// `avatars/avatars/…` from `GET /v1/avatars/{id}`. Planted **under** those
/// views (the composition wraps the raw store with it per request), this
/// rewrites all three to `avatars/…`; any other key passes through as it is.
/// The match is on whole segments, so `meother/avatars/…` is not rewritten.
pub struct AvatarRebind {
    inner: Arc<dyn Blob>,
}

impl AvatarRebind {
    /// The rebind over `inner`, the store the scoped views sit on.
    #[must_use]
    pub fn new(inner: Arc<dyn Blob>) -> Self {
        Self { inner }
    }

    /// The physical key for `key`.
    #[must_use]
    pub fn physical(key: &str) -> String {
        if let Some((module, rest)) = key.split_once('/')
            && AVATAR_MODULES.contains(&module)
            && let Some(tail) = rest.strip_prefix(AVATAR_PREFIX)
            && tail.starts_with('/')
        {
            return format!("{AVATAR_PREFIX}{tail}");
        }
        key.to_owned()
    }
}

#[async_trait]
impl Blob for AvatarRebind {
    async fn put(&self, key: &str, bytes: &[u8], content_type: &str) -> Result<(), BlobError> {
        self.inner
            .put(&Self::physical(key), bytes, content_type)
            .await
    }

    async fn get(&self, key: &str) -> Result<Option<BlobObject>, BlobError> {
        self.inner.get(&Self::physical(key)).await
    }

    async fn delete(&self, key: &str) -> Result<(), BlobError> {
        self.inner.delete(&Self::physical(key)).await
    }

    async fn signed_url(&self, key: &str, ttl: Duration) -> Result<String, BlobError> {
        self.inner.signed_url(&Self::physical(key), ttl).await
    }

    async fn signed_put_url(
        &self,
        key: &str,
        content_type: &str,
        content_length: Option<u64>,
        ttl: Duration,
    ) -> Result<PresignedPut, BlobError> {
        self.inner
            .signed_put_url(&Self::physical(key), content_type, content_length, ttl)
            .await
    }
}

// ---------------------------------------------------------------------------
// Rows

fn text(value: &str) -> Value {
    Value::String(Some(Box::new(value.to_owned())))
}

fn nullable(value: Option<&str>) -> Value {
    Value::String(value.map(|value| Box::new(value.to_owned())))
}

/// Sets (or, with `None`, clears) the name a live member chose. Answers
/// whether the member was found.
///
/// # Errors
///
/// [`DbError`] when the update fails.
pub async fn set_display_name(
    db: &dyn Database,
    workspace_id: &str,
    user_id: &str,
    display_name: Option<&str>,
    now: &str,
) -> Result<bool, DbError> {
    let statement = Statement::with_values(
        "UPDATE workspace_members SET display_name = ?, updated_at = ? \
          WHERE workspace_id = ? AND user_id = ? AND removed_at IS NULL",
        vec![
            nullable(display_name),
            text(now),
            text(workspace_id),
            text(user_id),
        ],
    );
    Ok(db.execute(&statement).await? > 0)
}

/// The avatar id a live member's row names, or `None`. `Ok(None)` is also a
/// member that is not there; [`set_member_avatar`] tells the two apart.
///
/// # Errors
///
/// [`DbError`] when the read fails.
pub async fn member_avatar(
    db: &dyn Database,
    workspace_id: &str,
    user_id: &str,
) -> Result<Option<String>, DbError> {
    let statement = Statement::with_values(
        "SELECT avatar FROM workspace_members \
          WHERE workspace_id = ? AND user_id = ? AND removed_at IS NULL",
        vec![text(workspace_id), text(user_id)],
    );
    Ok(db
        .query(&statement)
        .await?
        .first()
        .and_then(|row| row.get::<Option<String>>("avatar").flatten()))
}

/// Points a live member's row at `avatar` (or at none) and answers the id it
/// named before, for the caller to delete — `Ok(None)` when the member is
/// not there, `Ok(Some(None))` when they had no avatar.
///
/// # Errors
///
/// [`DbError`] when a statement fails.
pub async fn set_member_avatar(
    db: &dyn Database,
    workspace_id: &str,
    user_id: &str,
    avatar: Option<&str>,
    now: &str,
) -> Result<Option<Option<String>>, DbError> {
    let previous = member_avatar(db, workspace_id, user_id).await?;
    let statement = Statement::with_values(
        "UPDATE workspace_members SET avatar = ?, updated_at = ? \
          WHERE workspace_id = ? AND user_id = ? AND removed_at IS NULL",
        vec![
            nullable(avatar),
            text(now),
            text(workspace_id),
            text(user_id),
        ],
    );
    if db.execute(&statement).await? == 0 {
        return Ok(None);
    }
    Ok(Some(previous))
}

/// Points the workspace at `avatar` (or at none) and answers the id it named
/// before.
pub(crate) async fn set_workspace_avatar(
    db: &dyn Database,
    workspace_id: &str,
    avatar: Option<&str>,
) -> Result<Option<String>, DbError> {
    let read = Statement::with_values(
        "SELECT avatar FROM workspaces WHERE id = ?",
        vec![text(workspace_id)],
    );
    let previous = db
        .query(&read)
        .await?
        .first()
        .and_then(|row| row.get::<Option<String>>("avatar").flatten());
    let statement = Statement::with_values(
        "UPDATE workspaces SET avatar = ? WHERE id = ?",
        vec![nullable(avatar), text(workspace_id)],
    );
    db.execute(&statement).await?;
    Ok(previous)
}

/// Renames the workspace.
pub(crate) async fn set_workspace_name(
    db: &dyn Database,
    workspace_id: &str,
    name: &str,
) -> Result<(), DbError> {
    let statement = Statement::with_values(
        "UPDATE workspaces SET name = ? WHERE id = ?",
        vec![text(name), text(workspace_id)],
    );
    db.execute(&statement).await.map(|_| ())
}

/// Whether `avatar_id` is readable by a member of `workspace_id`: the
/// workspace's own avatar, or a **live** member's of the same workspace.
/// An avatar of another workspace, of a removed member, or one that has
/// since been replaced is not — and is answered exactly like one that was
/// never there.
///
/// # Errors
///
/// [`DbError`] when the read fails.
pub async fn readable_avatar(
    db: &dyn Database,
    workspace_id: &str,
    avatar_id: &str,
) -> Result<bool, DbError> {
    let statement = Statement::with_values(
        "SELECT 1 AS found FROM workspaces WHERE id = ? AND avatar = ? \
         UNION ALL \
         SELECT 1 AS found FROM workspace_members \
          WHERE workspace_id = ? AND avatar = ? AND removed_at IS NULL",
        vec![
            text(workspace_id),
            text(avatar_id),
            text(workspace_id),
            text(avatar_id),
        ],
    );
    Ok(!db.query(&statement).await?.rows.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(
            clean_name("  Ada Lovelace \n").as_deref(),
            Ok("Ada Lovelace")
        );
        assert_eq!(clean_name("   "), Err(NameError::Empty));
        assert_eq!(clean_name(""), Err(NameError::Empty));
        assert_eq!(clean_name(&"a".repeat(64)).map(|n| n.len()), Ok(64));
        assert_eq!(clean_name(&"a".repeat(65)), Err(NameError::TooLong));
        // Characters, not bytes: 64 emoji are a name.
        assert!(clean_name(&"🧠".repeat(64)).is_ok());
        assert_eq!(clean_name("Ada\u{0007}"), Err(NameError::Invisible));
        assert_eq!(clean_name("Ada\nLovelace"), Err(NameError::Invisible));
        assert_eq!(clean_name("Ada\u{202E}ecalevoL"), Err(NameError::Invisible));
        assert_eq!(clean_name("Ada\u{200B}"), Err(NameError::Invisible));
        assert!(clean_name("Zoë Ångström-李").is_ok());
    }

    #[test]
    fn avatar_ids_are_exactly_av_and_a_ulid() {
        assert!(is_avatar_id("av_01HZZZBBBBBBBBBBBBBBBBBBBB"));
        assert!(!is_avatar_id("av_01hzzzbbbbbbbbbbbbbbbbbbbb"));
        assert!(!is_avatar_id("av_01HZZZ"));
        assert!(!is_avatar_id("av_../../pages/x0000000000000"));
        assert!(!is_avatar_id("01HZZZBBBBBBBBBBBBBBBBBBBB"));
    }

    #[test]
    fn the_rebind_lands_every_module_on_one_space_and_nothing_else() {
        let id = "av_01HZZZBBBBBBBBBBBBBBBBBBBB";
        let want = format!("avatars/T1/{id}");
        for module in ["me", "workspaces", "avatars"] {
            assert_eq!(
                AvatarRebind::physical(&format!("{module}/avatars/T1/{id}")),
                want
            );
        }
        for untouched in [
            "pages/scope/body",
            "workspaces/other/x",
            "meother/avatars/T1/x",
            "notes/avatars/T1/x",
            "me/avatarsx/T1/x",
        ] {
            assert_eq!(AvatarRebind::physical(untouched), untouched);
        }
    }

    #[test]
    fn mime_parsing_ignores_parameters_and_refuses_the_rest() {
        assert_eq!(
            ImageKind::from_mime("image/PNG; charset=x"),
            Some(ImageKind::Png)
        );
        assert_eq!(ImageKind::from_mime("image/jpg"), Some(ImageKind::Jpeg));
        assert_eq!(ImageKind::from_mime("image/svg+xml"), None);
        assert_eq!(ImageKind::from_mime("image/gif"), None);
        assert_eq!(ImageKind::from_mime("text/html"), None);
    }
}
