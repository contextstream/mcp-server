//! Brand work by name: the forms of brand images and brand sounds a person or an agent asks
//! for, the sizes an image is delivered in and the voices that speak, as Create advertises
//! them (`catalog.make.forms`, `sizes`, `voices`; contextstudio
//! `packages/producer/src/forms.ts`), and how a plan's and a made piece's brand work reads:
//! its sizes and files, its alt text and caption, its checks.
//!
//! Pure functions over the tool's input and Create's answers; `mod.rs` calls them from
//! `plan`, `result` and `revise`.

use mcp_types::{Error, Result};
use serde_json::{json, Value};

/// What a form is made as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Medium {
    Image,
    Sound,
}

/// One form of brand work.
#[derive(Debug)]
pub(crate) struct Form {
    pub id: &'static str,
    pub medium: Medium,
    pub label: &'static str,
    /// What it is, in a few words, for the schema an agent picks from.
    pub hint: &'static str,
    /// A printed sheet: its paper size and a print PDF, never screen sizes.
    pub print: bool,
    /// A voice says it.
    pub speaks: bool,
}

const fn form(id: &'static str, medium: Medium, label: &'static str, hint: &'static str) -> Form {
    Form {
        id,
        medium,
        label,
        hint,
        print: false,
        speaks: false,
    }
}

const fn printed(id: &'static str, label: &'static str, hint: &'static str) -> Form {
    Form {
        print: true,
        ..form(id, Medium::Image, label, hint)
    }
}

const fn spoken(id: &'static str, label: &'static str, hint: &'static str) -> Form {
    Form {
        speaks: true,
        ..form(id, Medium::Sound, label, hint)
    }
}

pub(crate) const FORMS: &[Form] = &[
    form(
        "social-set",
        Medium::Image,
        "Social set",
        "one post in each network's size",
    ),
    form(
        "ad-set",
        Medium::Image,
        "Ad set",
        "ad creatives in the ad networks' sizes, the offer inside every safe area",
    ),
    form(
        "hero",
        Medium::Image,
        "Hero image",
        "a wide image for a website or launch page, with room for a headline",
    ),
    form(
        "product-shot",
        Medium::Image,
        "Product shot",
        "a studio shot of the product, best with a photo of it attached",
    ),
    printed(
        "poster",
        "Poster",
        "a printed sheet in the brand's type, with a print-ready PDF",
    ),
    printed(
        "flyer",
        "Flyer",
        "an A5 or US Letter sheet, with a print-ready PDF",
    ),
    form(
        "email-header",
        Medium::Image,
        "Email header",
        "a header band for emails and newsletters",
    ),
    form(
        "brand-graphic",
        Medium::Image,
        "Brand graphic",
        "a graphic from the brand's colour, pattern and mark",
    ),
    form(
        "sonic-logo",
        Medium::Sound,
        "Sonic logo",
        "a 2 to 8 second sound that says the brand",
    ),
    form(
        "music-bed",
        Medium::Sound,
        "Music bed",
        "instrumental music under a voice, a video or an ad",
    ),
    form(
        "loop",
        Medium::Sound,
        "Loop",
        "music that repeats without a seam, for a site or a booth",
    ),
    form(
        "sfx-pack",
        Medium::Sound,
        "Sound effects pack",
        "2 to 6 matching sounds, such as tap, success, alert",
    ),
    spoken(
        "voice-over",
        "Voice-over",
        "the team's words read in a chosen voice",
    ),
    spoken(
        "podcast-intro",
        "Podcast intro",
        "a voice over a music bed, landing on a sting",
    ),
    spoken(
        "podcast-outro",
        "Podcast outro",
        "thanks, where to find the show, and the sting",
    ),
];

/// An image's frame where it runs.
#[derive(Debug)]
pub(crate) struct Size {
    pub id: &'static str,
    pub label: &'static str,
    pub width: u32,
    pub height: u32,
}

const fn size(id: &'static str, label: &'static str, width: u32, height: u32) -> Size {
    Size {
        id,
        label,
        width,
        height,
    }
}

pub(crate) const SIZES: &[Size] = &[
    size("instagram-post", "Instagram post", 1080, 1350),
    size("instagram-square", "Instagram square", 1080, 1080),
    size("instagram-story", "Instagram story", 1080, 1920),
    size("facebook-post", "Facebook post", 1080, 1350),
    size("linkedin-post", "LinkedIn post", 1200, 1200),
    size("linkedin-wide", "LinkedIn landscape", 1200, 627),
    size("x-post", "X post", 1600, 900),
    size("pinterest-pin", "Pinterest pin", 1000, 1500),
    size("tiktok-cover", "TikTok cover", 1080, 1920),
    size("youtube-thumbnail", "YouTube thumbnail", 1280, 720),
    size("ad-square", "Square ad", 1080, 1080),
    size("ad-portrait", "Portrait ad", 1080, 1350),
    size("ad-story", "Story ad", 1080, 1920),
    size("ad-landscape", "Landscape ad", 1200, 628),
    size("ad-rectangle", "Display rectangle, 300 × 250", 600, 500),
    size("ad-half-page", "Display half page, 300 × 600", 600, 1200),
    size("web-hero", "Website hero", 2400, 1350),
    size("web-banner", "Wide website banner", 2520, 1080),
    size("link-preview", "Link preview", 1200, 630),
    size("product-square", "Product shot, square", 2048, 2048),
    size("product-portrait", "Product shot, portrait", 1600, 2000),
    size("email-header", "Email header", 1200, 400),
    size("email-hero", "Email hero", 1200, 600),
];

/// The most sizes one image is delivered in.
pub(crate) const MAX_SIZES: usize = 6;

/// The curated voices, with how each reads.
pub(crate) const VOICES: &[(&str, &str)] = &[
    ("warm-narrator", "warm and unhurried"),
    ("trailer", "deep and dramatic"),
    ("presenter", "clear and steady, for walkthroughs"),
    ("bright-social", "bright and upbeat, for social"),
    ("calm-guide", "soft and reassuring"),
    ("launch", "energetic, for ads and launches"),
    ("corporate", "friendly and clear"),
    ("storyteller", "a storyteller's read"),
    ("conversational", "relaxed, like talking to a friend"),
    ("documentary", "measured, documentary"),
    ("playful", "light and playful"),
    ("gentle", "quiet and gentle"),
];

/// The most files listed, each with its own signed link, for one made piece.
pub(crate) const FILES_PER_PIECE: usize = 12;

pub(crate) fn form_ids() -> Vec<&'static str> {
    FORMS.iter().map(|f| f.id).collect()
}

pub(crate) fn size_ids() -> Vec<&'static str> {
    SIZES.iter().map(|s| s.id).collect()
}

pub(crate) fn voice_keys() -> Vec<&'static str> {
    VOICES.iter().map(|(key, _)| *key).collect()
}

/// "Social Set", "social_set" and "social-set" are one key.
fn key_of(raw: &str) -> String {
    raw.trim()
        .to_lowercase()
        .split(|c: char| c.is_whitespace() || c == '_' || c == '-')
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

pub(crate) fn find_form(id: &str) -> Option<&'static Form> {
    let key = key_of(id);
    FORMS.iter().find(|f| f.id == key || key_of(f.label) == key)
}

pub(crate) fn find_size(id: &str) -> Option<&'static Size> {
    let key = key_of(id);
    SIZES.iter().find(|s| s.id == key || key_of(s.label) == key)
}

/// A form's name, for one Create may have added since this was written too.
pub(crate) fn form_label(id: &str) -> String {
    match find_form(id) {
        Some(form) => form.label.to_string(),
        None => capitalized(&id.replace(['-', '_'], " ")),
    }
}

// ---------------------------------------------------------------------------------------
// What the agent asks for
// ---------------------------------------------------------------------------------------

/// The form asked for, if any.
pub(crate) fn parse_form(raw: Option<&str>) -> Result<Option<&'static Form>> {
    let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
        return Ok(None);
    };
    find_form(raw).map(Some).ok_or_else(|| {
        Error::Validation(format!(
            "Unknown form “{raw}”. Use one of: {}; or leave form out and the producer chooses.",
            FORMS
                .iter()
                .map(|f| format!("{} ({})", f.id, f.hint))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })
}

/// The sizes asked for: ids (or their names), as a list or one comma-separated string,
/// each once, at most [`MAX_SIZES`].
pub(crate) fn parse_sizes(raw: Option<&Value>) -> Result<Vec<&'static Size>> {
    let asked: Vec<String> = match raw {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::String(text)) => text
            .split([',', ';'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(id) => Ok(id.trim().to_string()),
                Value::Object(map) => map
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|id| id.trim().to_string())
                    .ok_or_else(|| sizes_problem("Each size is an id, such as instagram-post.")),
                _ => Err(sizes_problem("Each size is an id, such as instagram-post.")),
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect(),
        Some(_) => {
            return Err(sizes_problem(
                "sizes is a list of size ids, such as [\"instagram-post\", \"linkedin-post\"].",
            ))
        }
    };
    let mut sizes: Vec<&'static Size> = Vec::new();
    for id in &asked {
        let size = find_size(id).ok_or_else(|| sizes_problem(&format!("Unknown size “{id}”.")))?;
        if !sizes.iter().any(|s| s.id == size.id) {
            sizes.push(size);
        }
    }
    if sizes.len() > MAX_SIZES {
        return Err(Error::Validation(format!(
            "Up to {MAX_SIZES} sizes for one image; {} were asked. The first is the master shape.",
            sizes.len()
        )));
    }
    Ok(sizes)
}

fn sizes_problem(said: &str) -> Error {
    Error::Validation(format!(
        "{said} Sizes: {}.",
        SIZES
            .iter()
            .map(|s| format!("{} ({}×{})", s.id, s.width, s.height))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// The voice asked for, if any.
pub(crate) fn parse_voice(raw: Option<&str>) -> Result<Option<&'static str>> {
    let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
        return Ok(None);
    };
    let key = key_of(raw);
    VOICES
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(k, _)| Some(*k))
        .ok_or_else(|| {
            Error::Validation(format!(
                "Unknown voice “{raw}”. Voices: {}.",
                VOICES
                    .iter()
                    .map(|(k, how)| format!("{k} ({how})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
}

/// Whether a form, sizes and a voice go together, said plainly when they don't.
pub(crate) fn check_together(
    form: Option<&Form>,
    sizes: &[&Size],
    voice: Option<&str>,
) -> Result<()> {
    let Some(form) = form else {
        return Ok(());
    };
    if !sizes.is_empty() {
        if form.medium == Medium::Sound {
            return Err(Error::Validation(format!(
                "A {} is a sound: sizes are for an image form ({}).",
                form.label.to_lowercase(),
                sized_forms().join(", ")
            )));
        }
        if form.print {
            return Err(Error::Validation(format!(
                "A {} is a printed sheet with its own paper size and a print PDF, so it takes no sizes. For screens, ask for a social-set or a brand-graphic.",
                form.label.to_lowercase()
            )));
        }
    }
    if voice.is_some() && !form.speaks {
        return Err(Error::Validation(format!(
            "A {} has no voice: voice is for {} (or no form).",
            form.label.to_lowercase(),
            FORMS
                .iter()
                .filter(|f| f.speaks)
                .map(|f| f.id)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(())
}

fn sized_forms() -> Vec<&'static str> {
    FORMS
        .iter()
        .filter(|f| f.medium == Medium::Image && !f.print)
        .map(|f| f.id)
        .collect()
}

/// Put what was asked into Create's request, at its top level.
pub(crate) fn ask(request: &mut Value, form: Option<&Form>, sizes: &[&Size], voice: Option<&str>) {
    if let Some(form) = form {
        request["form"] = json!(form.id);
    }
    if !sizes.is_empty() {
        request["sizes"] = json!(size_ids_of(sizes));
    }
    if let Some(voice) = voice {
        request["voice"] = json!(voice);
    }
}

// ---------------------------------------------------------------------------------------
// revise: start from a made piece
// ---------------------------------------------------------------------------------------

/// How a revision starts from a made piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// More like it.
    Variation,
    /// It, changed as the change says.
    Revision,
    /// It in new sizes.
    Resize,
}

pub(crate) const MODES: &[&str] = &["variation", "revision", "resize"];

impl Mode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Variation => "variation",
            Self::Revision => "revision",
            Self::Resize => "resize",
        }
    }

    pub(crate) fn parse(raw: Option<&str>) -> Result<Option<Self>> {
        let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
            return Ok(None);
        };
        match key_of(raw).as_str() {
            "variation" | "vary" | "more-like-this" => Ok(Some(Self::Variation)),
            "revision" | "revise" | "change" => Ok(Some(Self::Revision)),
            "resize" => Ok(Some(Self::Resize)),
            _ => Err(Error::Validation(format!(
                "Unknown mode “{raw}”: use variation (more like it), revision (it, changed as change says) or resize (it in new sizes)."
            ))),
        }
    }

    /// The mode when none is named: a resize when only sizes are given, a revision when
    /// there is a change, else a variation.
    pub(crate) fn implied(change: Option<&str>, sizes: &[&Size]) -> Self {
        match (change, sizes.is_empty()) {
            (Some(_), _) => Self::Revision,
            (None, false) => Self::Resize,
            (None, true) => Self::Variation,
        }
    }
}

/// The prompt for a plan that starts from a made piece: the change itself, "More like
/// this", or "Resize for …".
pub(crate) fn from_prompt(mode: Mode, change: Option<&str>, sizes: &[&Size]) -> String {
    match (mode, change) {
        (_, Some(change)) => change.to_string(),
        (Mode::Resize, None) => format!(
            "Resize for {}",
            and_list(sizes.iter().map(|s| s.label.to_string()).collect())
        ),
        _ => "More like this".to_string(),
    }
}

/// What a plan from a made piece is, for the note under it.
pub(crate) fn from_words(mode: Mode, title: &str, sizes: &[&Size]) -> String {
    match mode {
        Mode::Variation => format!("more like “{title}”"),
        Mode::Revision => format!("“{title}”, changed as asked"),
        Mode::Resize => format!(
            "“{title}” in {}",
            and_list(sizes.iter().map(|s| s.label.to_string()).collect())
        ),
    }
}

// ---------------------------------------------------------------------------------------
// A plan's piece
// ---------------------------------------------------------------------------------------

/// "Instagram post 1080×1350, Instagram story 1080×1920", from a piece's sizes (objects
/// with a label and pixels, or ids).
pub(crate) fn sizes_words(sizes: Option<&Value>) -> Option<String> {
    let words: Vec<String> = sizes?
        .as_array()?
        .iter()
        .filter_map(|item| {
            let (id, label, width, height) = match item {
                Value::String(id) => (id.as_str(), None, None, None),
                Value::Object(map) => (
                    map.get("id").and_then(Value::as_str).unwrap_or(""),
                    map.get("label").and_then(Value::as_str),
                    map.get("width").and_then(Value::as_u64),
                    map.get("height").and_then(Value::as_u64),
                ),
                _ => return None,
            };
            let known = find_size(id);
            let label = label
                .map(str::to_string)
                .or_else(|| known.map(|s| s.label.to_string()))
                .unwrap_or_else(|| id.to_string());
            if label.is_empty() {
                return None;
            }
            let pixels = width
                .zip(height)
                .or_else(|| known.map(|s| (u64::from(s.width), u64::from(s.height))));
            Some(match pixels {
                Some((w, h)) => format!("{label} {w}×{h}"),
                None => label,
            })
        })
        .collect();
    (!words.is_empty()).then(|| words.join(", "))
}

fn format_name(format: &str) -> String {
    match format {
        "png" => "PNG".to_string(),
        "jpg" | "jpeg" => "JPG".to_string(),
        "webp" => "WebP".to_string(),
        "pdf" => "PDF".to_string(),
        "mp3" => "MP3".to_string(),
        "wav" => "WAV".to_string(),
        "zip" => "ZIP".to_string(),
        other => other.to_uppercase(),
    }
}

/// What a planned piece is delivered as: "PNG, JPG, WebP and a ZIP of the set"; "WAV and
/// MP3 at −16 LUFS for the web, a −23 LUFS broadcast master, and stems".
pub(crate) fn exports_words(piece: &Value) -> Option<String> {
    let exports: Vec<String> = piece
        .get("exports")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    if exports.is_empty() {
        return None;
    }
    let has = |format: &str| exports.iter().any(|e| e == format);
    let medium = piece.get("medium").and_then(Value::as_str).unwrap_or("");
    let sound = medium == "sound" || has("mp3") || has("wav");
    let mut parts: Vec<String> = Vec::new();
    let known: &[&str] = if sound {
        let audio: Vec<String> = ["wav", "mp3"]
            .into_iter()
            .filter(|f| has(f))
            .map(format_name)
            .collect();
        let pack = piece.get("count").is_some_and(|c| !c.is_null())
            || piece.get("form").and_then(Value::as_str) == Some("sfx-pack");
        if pack {
            if !audio.is_empty() {
                parts.push(format!("each sound as {}", and_list(audio)));
            }
            if has("zip") {
                parts.push("a ZIP of the pack".to_string());
            }
        } else {
            if !audio.is_empty() {
                parts.push(format!("{} at −16 LUFS for the web", and_list(audio)));
            }
            if has("broadcast") {
                parts.push("a −23 LUFS broadcast master".to_string());
            }
            if has("stems") {
                parts.push("stems".to_string());
            }
            if has("zip") {
                parts.push("a ZIP".to_string());
            }
        }
        &["wav", "mp3", "broadcast", "stems", "zip"]
    } else {
        if has("pdf") {
            parts.push("a print-ready PDF".to_string());
        }
        for format in ["png", "jpg", "webp"] {
            if has(format) {
                parts.push(format_name(format));
            }
        }
        if has("zip") {
            parts.push("a ZIP of the set".to_string());
        }
        &["pdf", "png", "jpg", "jpeg", "webp", "zip"]
    };
    for other in exports.iter().filter(|e| !known.contains(&e.as_str())) {
        parts.push(format_name(other));
    }
    (!parts.is_empty()).then(|| capitalized(&and_list(parts)))
}

/// The voice a piece speaks in: its name, else its key.
pub(crate) fn voice_name(voice: Option<&Value>) -> Option<String> {
    match voice? {
        Value::String(key) if !key.trim().is_empty() => Some(capitalized(&key.replace('-', " "))),
        Value::Object(map) => map
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.trim().is_empty())
            .map(str::to_string)
            .or_else(|| {
                map.get("key")
                    .and_then(Value::as_str)
                    .map(|k| capitalized(&k.replace('-', " ")))
            }),
        _ => None,
    }
}

fn mode_of(from: Option<&Value>) -> Option<&str> {
    match from? {
        Value::String(mode) => Some(mode.as_str()),
        Value::Object(map) => map.get("mode").and_then(Value::as_str),
        _ => None,
    }
}

/// A planned or made piece that starts from a made one, in words.
fn started_from(from: Option<&Value>) -> Option<&'static str> {
    Some(match mode_of(from)? {
        "variation" => "From a made piece: more like it.",
        "revision" => "From a made piece: it, changed as asked.",
        "resize" => "From a made piece: the same picture in new sizes.",
        _ => return None,
    })
}

/// A planned piece's brand work, one line each, under its title.
pub(crate) fn plan_lines(piece: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(sizes) = sizes_words(piece.get("sizes")) {
        lines.push(format!("   Sizes: {sizes}."));
    }
    if let Some(files) = exports_words(piece) {
        lines.push(format!("   Files: {files}."));
    }
    if let Some(voice) = voice_name(piece.get("voice")) {
        lines.push(format!("   Voice: {voice}."));
    }
    if let Some(from) = started_from(piece.get("from")) {
        lines.push(format!("   {from}"));
    }
    lines
}

/// A planned piece's brand work for the structured result, or `None` for a piece with none.
pub(crate) fn plan_delivery(piece: &Value) -> Option<Value> {
    let form = piece.get("form").and_then(Value::as_str);
    let sizes = sizes_words(piece.get("sizes"));
    let files = exports_words(piece);
    let voice = voice_name(piece.get("voice"));
    let from = mode_of(piece.get("from"));
    if form.is_none() && sizes.is_none() && files.is_none() && voice.is_none() && from.is_none() {
        return None;
    }
    Some(json!({
        "form": form,
        "form_label": form.map(form_label),
        "sizes": sizes,
        "files": files,
        "voice": voice,
        "sounds": piece.get("count"),
        "from": from,
    }))
}

// ---------------------------------------------------------------------------------------
// A made piece
// ---------------------------------------------------------------------------------------

/// A file's own signed link, as Create gave it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SignedFile {
    pub url: String,
    pub expires_in_seconds: Option<i64>,
    pub size_bytes: Option<u64>,
}

fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value.get(field).and_then(Value::as_str).unwrap_or("")
}

/// How useful a file is to list first: each size's PNG, the set ZIP and a print PDF for an
/// image; the web MP3, the broadcast WAV and the stems or pack ZIP for a sound.
fn file_rank(file: &Value, sound: bool) -> u8 {
    let format = text(file, "format").to_lowercase();
    let role = text(file, "role");
    if sound {
        match (format.as_str(), role) {
            ("mp3", "web") => 0,
            ("wav", "broadcast") => 1,
            ("zip", _) => 2,
            ("wav", "sound") => 3,
            ("mp3", "sound") => 4,
            ("wav", "web") => 5,
            ("mp3", "broadcast") => 6,
            _ => 7,
        }
    } else {
        match format.as_str() {
            "pdf" => 0,
            "png" => 1,
            "zip" => 2,
            "jpg" | "jpeg" => 3,
            "webp" => 4,
            _ => 5,
        }
    }
}

/// A made piece's files, the most useful first: the ones to link (at most `max`) and the
/// rest.
pub(crate) fn chosen_files(files: &[Value], medium: &str, max: usize) -> (Vec<Value>, Vec<Value>) {
    let sound = medium == "sound";
    let mut ranked: Vec<Value> = files
        .iter()
        .filter(|f| f.get("id").and_then(Value::as_str).is_some())
        .cloned()
        .collect();
    ranked.sort_by_key(|f| file_rank(f, sound));
    let rest = if ranked.len() > max {
        ranked.split_off(max)
    } else {
        Vec::new()
    };
    (ranked, rest)
}

/// Whether a file is one of a piece's main downloads, linked as its own resource: the set
/// ZIP or print PDF of an image; the web MP3, broadcast WAV and stems or pack ZIP of a sound.
pub(crate) fn is_main_download(file: &Value, medium: &str) -> bool {
    if medium == "sound" {
        file_rank(file, true) <= 2
    } else {
        matches!(text(file, "format").to_lowercase().as_str(), "zip" | "pdf")
    }
}

/// "1.2 MB", "96 KB".
pub(crate) fn bytes_words(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{} KB", ((bytes + 512) / 1024).max(1))
    }
}

/// A file's label: Create's, else its name.
pub(crate) fn file_label(file: &Value) -> String {
    let label = text(file, "label").trim();
    if !label.is_empty() {
        return label.to_string();
    }
    let name = text(file, "name").trim();
    if !name.is_empty() {
        return name.to_string();
    }
    text(file, "id").to_string()
}

/// "   - Instagram post · PNG · 1080×1350 · 1.2 MB: https://…".
pub(crate) fn file_line(file: &Value, signed: Option<&SignedFile>) -> String {
    let mut facts: Vec<String> = Vec::new();
    if let (Some(w), Some(h)) = (
        file.get("width").and_then(Value::as_u64),
        file.get("height").and_then(Value::as_u64),
    ) {
        facts.push(format!("{w}×{h}"));
    }
    if let Some(bytes) = file
        .get("sizeBytes")
        .and_then(Value::as_u64)
        .or_else(|| signed.and_then(|s| s.size_bytes))
    {
        facts.push(bytes_words(bytes));
    }
    let facts: String = facts.iter().map(|fact| format!(" · {fact}")).collect();
    match signed {
        Some(signed) => format!("   - {}{facts}: {}", file_label(file), signed.url),
        None => format!(
            "   - {}{facts}: no link could be made just now; it is in the ContextStudio library",
            file_label(file)
        ),
    }
}

/// The files not linked, in one line.
pub(crate) fn more_files_line(rest: &[Value], all: &[Value]) -> Option<String> {
    if rest.is_empty() {
        return None;
    }
    let mut formats: Vec<String> = Vec::new();
    for file in rest {
        let name = format_name(&text(file, "format").to_lowercase());
        if !name.is_empty() && !formats.contains(&name) {
            formats.push(name);
        }
    }
    let zipped = all
        .iter()
        .any(|f| text(f, "format").eq_ignore_ascii_case("zip"))
        && rest
            .iter()
            .all(|f| !text(f, "format").eq_ignore_ascii_case("zip"));
    Some(format!(
        "   And {} more file{}{}, {}.",
        rest.len(),
        if rest.len() == 1 { "" } else { "s" },
        if formats.is_empty() {
            String::new()
        } else {
            format!(" ({})", and_list(formats))
        },
        if zipped {
            "all in the ZIP"
        } else {
            "in the ContextStudio library"
        }
    ))
}

/// A file as the structured result lists it, without the fields it doesn't have.
pub(crate) fn file_entry(file: &Value, signed: Option<&SignedFile>) -> Value {
    let mut entry = json!({
        "id": file.get("id"),
        "label": file_label(file),
        "format": file.get("format"),
        "mime_type": file.get("mimeType"),
        "url": signed.map(|s| s.url.clone()),
        "expires_in_seconds": signed.and_then(|s| s.expires_in_seconds),
        "size": file.get("size"),
        "width": file.get("width"),
        "height": file.get("height"),
        "size_bytes": file.get("sizeBytes").cloned().or_else(|| signed.and_then(|s| s.size_bytes).map(Value::from)),
        "duration_ms": file.get("durationMs"),
        "role": file.get("role"),
        "loudness": file.get("loudness"),
    });
    // Only what the file has, but always whether it has a link.
    if let Some(map) = entry.as_object_mut() {
        map.retain(|key, value| key == "url" || !value.is_null());
    }
    entry
}

/// A size's name in a piece: the piece's own label for it, else the catalog's, else its id.
fn size_label(id: &str, sizes: Option<&Value>) -> String {
    sizes
        .and_then(Value::as_array)
        .and_then(|list| {
            list.iter()
                .find(|s| s.get("id").and_then(Value::as_str) == Some(id))
                .and_then(|s| s.get("label").and_then(Value::as_str))
                .map(str::to_string)
        })
        .or_else(|| find_size(id).map(|s| s.label.to_string()))
        .unwrap_or_else(|| id.to_string())
}

/// The checks that didn't pass, one phrase each: "Type inside the safe area (Instagram
/// story): the headline runs under the reply bar".
pub(crate) fn failed_checks(checks: &[Value], sizes: Option<&Value>) -> Vec<String> {
    checks
        .iter()
        .filter(|c| c.get("ok").and_then(Value::as_bool) == Some(false))
        .map(|check| {
            let name = text(check, "name").trim();
            let name = if name.is_empty() { "A check" } else { name };
            let mut words = name.to_string();
            if let Some(size) = check.get("size").and_then(Value::as_str) {
                let label = size_label(size, sizes);
                if !name.to_lowercase().contains(&label.to_lowercase()) {
                    words.push_str(&format!(" ({label})"));
                }
            }
            let detail = text(check, "detail").trim().trim_end_matches('.');
            if !detail.is_empty() {
                words.push_str(&format!(": {detail}"));
            }
            words
        })
        .collect()
}

/// "Checks: all 8 passed." or "Checks: 7 of 8 passed. Not passed: …".
pub(crate) fn checks_line(checks: &[Value], sizes: Option<&Value>) -> Option<String> {
    if checks.is_empty() {
        return None;
    }
    let failed = failed_checks(checks, sizes);
    let count = checks.len();
    if failed.is_empty() {
        return Some(if count == 1 {
            "   Checks: the one check passed.".to_string()
        } else {
            format!("   Checks: all {count} passed.")
        });
    }
    const NAMED: usize = 4;
    let mut named: Vec<String> = failed.iter().take(NAMED).cloned().collect();
    if failed.len() > NAMED {
        named.push(format!("and {} more", failed.len() - NAMED));
    }
    Some(format!(
        "   Checks: {} of {count} passed. Not passed: {}.",
        count - failed.len(),
        named.join("; ")
    ))
}

/// A pack's sounds: "Tap 0.4 s, Success 1.2 s and Alert 0.9 s".
pub(crate) fn pack_words(pack: Option<&Value>) -> Option<String> {
    let sounds: Vec<String> = pack?
        .as_array()?
        .iter()
        .filter_map(|sound| {
            let title = sound.get("title").and_then(Value::as_str)?.trim();
            if title.is_empty() {
                return None;
            }
            Some(match sound.get("seconds").and_then(Value::as_f64) {
                Some(seconds) => format!("{title} {} s", trimmed_number(seconds)),
                None => title.to_string(),
            })
        })
        .collect();
    (!sounds.is_empty()).then(|| and_list(sounds))
}

fn trimmed_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value:.1}")
    }
}

/// A made piece's brand work, one line each, before its files: alt text, caption, sizes,
/// sounds, voice and where it started from.
pub(crate) fn made_lines(piece: &Value, made: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(alt) = flat(made.get("alt")) {
        lines.push(format!("   Alt text: {alt}"));
    }
    if let Some(caption) = flat(made.get("caption")) {
        lines.push(format!("   Caption: {caption}"));
    }
    if let Some(sizes) = sizes_words(made.get("sizes")) {
        lines.push(format!("   Sizes: {sizes}."));
    }
    if let Some(sounds) = pack_words(made.get("pack")) {
        lines.push(format!("   Sounds: {sounds}."));
    }
    if let Some(voice) = voice_name(made.get("voice").or_else(|| piece.get("voice"))) {
        lines.push(format!("   Voice: {voice}."));
    }
    if let Some(from) = started_from(made.get("from")) {
        lines.push(format!("   {from}"));
    }
    lines
}

/// A made piece's brand work for the structured result.
pub(crate) fn made_entry(entry: &mut Value, piece: &Value, made: &Value) {
    let Some(map) = entry.as_object_mut() else {
        return;
    };
    let mut put = |key: &str, value: Option<Value>| {
        if let Some(value) = value.filter(|v| !v.is_null()) {
            map.insert(key.to_string(), value);
        }
    };
    put("form", piece.get("form").cloned());
    put("alt", made.get("alt").cloned());
    put("caption", made.get("caption").cloned());
    put("sizes", made.get("sizes").cloned());
    put("pack", made.get("pack").cloned());
    put(
        "voice",
        made.get("voice").or_else(|| piece.get("voice")).cloned(),
    );
    put("from", made.get("from").cloned());
    if let Some(checks) = made.get("checks").and_then(Value::as_array) {
        put("checks", Some(Value::Array(checks.clone())));
        put(
            "checks_failed",
            Some(json!(failed_checks(checks, made.get("sizes")))),
        );
    }
}

fn flat(value: Option<&Value>) -> Option<String> {
    let text = value?
        .as_str()?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

// ---------------------------------------------------------------------------------------
// Words
// ---------------------------------------------------------------------------------------

/// "a, b and c"; with a comma before the last "and" when an item has its own "and".
pub(crate) fn and_list(items: Vec<String>) -> String {
    let inner_and = items.iter().any(|i| i.contains(" and "));
    match items.len() {
        0 => String::new(),
        1 => items[0].clone(),
        n => format!(
            "{}{} and {}",
            items[..n - 1].join(", "),
            if inner_and { "," } else { "" },
            items[n - 1]
        ),
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The form parameter's description: each form with a few words.
pub(crate) fn form_description() -> String {
    let (images, sounds): (Vec<&Form>, Vec<&Form>) =
        FORMS.iter().partition(|f| f.medium == Medium::Image);
    let list = |forms: Vec<&Form>| {
        forms
            .iter()
            .map(|f| format!("{}: {}", f.id, f.hint))
            .collect::<Vec<_>>()
            .join("; ")
    };
    format!(
        "Brand work by name, for an exact plan (plan). Brand images: {}. Brand sounds: {}. Leave it out for films and anything else; the producer then chooses.",
        list(images),
        list(sounds)
    )
}

/// The voice parameter's description.
pub(crate) fn voice_description() -> String {
    format!(
        "The voice that speaks (plan, revise), for voice-over, podcast-intro, podcast-outro or no form: {}. Default: the brand kit's narrator.",
        VOICES
            .iter()
            .map(|(k, how)| format!("{k} ({how})"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The sizes parameter's schema.
pub(crate) fn sizes_schema() -> Value {
    json!({
        "type": "array",
        "description": format!(
            "Image sizes by id, at most {MAX_SIZES}; the first is the master shape (plan, with an image form other than poster or flyer; revise, for a resize). Default: the form's own. Common: instagram-post (1080×1350), instagram-story (1080×1920), linkedin-post (1200×1200), linkedin-wide (1200×627), x-post (1600×900), ad-square, ad-story, ad-rectangle (300×250 at 2x), web-hero (2400×1350), link-preview (1200×630), email-header (1200×400)."
        ),
        "items": {"type": "string", "enum": size_ids()},
    })
}

/// The structured form of a list of sizes asked for.
pub(crate) fn size_ids_of(sizes: &[&Size]) -> Vec<&'static str> {
    sizes.iter().map(|s| s.id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_matches_creates() {
        assert_eq!(FORMS.len(), 15);
        assert_eq!(SIZES.len(), 23);
        assert_eq!(VOICES.len(), 12);
        assert_eq!(
            FORMS.iter().filter(|f| f.medium == Medium::Sound).count(),
            7
        );
        assert_eq!(find_form("Social set").unwrap().id, "social-set");
        assert_eq!(find_form("sound effects pack").unwrap().id, "sfx-pack");
        assert_eq!(find_size("Instagram Story").unwrap().id, "instagram-story");
        assert_eq!(find_size("linkedin_wide").unwrap().width, 1200);
        assert_eq!(form_label("tote-bag"), "Tote bag");
    }

    #[test]
    fn what_is_asked_is_read_loosely_and_checked_plainly() {
        let sizes = parse_sizes(Some(&json!(
            "Instagram post, instagram-story, instagram-post"
        )))
        .unwrap();
        assert_eq!(size_ids_of(&sizes), ["instagram-post", "instagram-story"]);
        let unknown = parse_sizes(Some(&json!(["instagram-reel"]))).unwrap_err();
        assert!(unknown
            .to_string()
            .contains("Unknown size “instagram-reel”"));
        assert!(unknown.to_string().contains("x-post (1600×900)"));
        let seven: Vec<&str> = SIZES.iter().take(7).map(|s| s.id).collect();
        assert!(parse_sizes(Some(&json!(seven)))
            .unwrap_err()
            .to_string()
            .contains("Up to 6 sizes"));
        assert!(parse_form(Some("banner"))
            .unwrap_err()
            .to_string()
            .contains("social-set (one post in each network's size)"));
        assert_eq!(
            parse_voice(Some("Warm narrator")).unwrap(),
            Some("warm-narrator")
        );
        assert!(parse_voice(Some("robot"))
            .unwrap_err()
            .to_string()
            .contains("calm-guide (soft and reassuring)"));

        let sonic = find_form("sonic-logo");
        let story = vec![find_size("instagram-story").unwrap()];
        assert!(check_together(sonic, &story, None)
            .unwrap_err()
            .to_string()
            .contains("sizes are for an image form"));
        assert!(check_together(find_form("poster"), &story, None)
            .unwrap_err()
            .to_string()
            .contains("printed sheet"));
        assert!(
            check_together(find_form("social-set"), &story, Some("gentle"))
                .unwrap_err()
                .to_string()
                .contains("voice is for voice-over, podcast-intro, podcast-outro")
        );
        assert!(check_together(find_form("voice-over"), &[], Some("gentle")).is_ok());
        assert!(check_together(None, &story, Some("gentle")).is_ok());

        assert_eq!(Mode::parse(Some("Resize")).unwrap(), Some(Mode::Resize));
        assert!(Mode::parse(Some("crop")).is_err());
        assert_eq!(Mode::implied(None, &story), Mode::Resize);
        assert_eq!(Mode::implied(Some("warmer"), &story), Mode::Revision);
        assert_eq!(Mode::implied(None, &[]), Mode::Variation);
        let two = vec![
            find_size("x-post").unwrap(),
            find_size("link-preview").unwrap(),
        ];
        assert_eq!(
            from_prompt(Mode::Resize, None, &two),
            "Resize for X post and Link preview"
        );
        assert_eq!(from_prompt(Mode::Variation, None, &[]), "More like this");
    }

    #[test]
    fn a_plans_brand_work_reads_in_words() {
        let set = json!({
            "medium": "image", "form": "social-set",
            "sizes": [
                {"id": "instagram-post", "label": "Instagram post", "width": 1080, "height": 1350},
                {"id": "instagram-story", "label": "Instagram story", "width": 1080, "height": 1920},
                "linkedin-post",
            ],
            "exports": ["png", "jpg", "webp", "zip"],
        });
        assert_eq!(
            plan_lines(&set),
            [
                "   Sizes: Instagram post 1080×1350, Instagram story 1080×1920, LinkedIn post 1200×1200.",
                "   Files: PNG, JPG, WebP and a ZIP of the set.",
            ]
        );
        let sting = json!({"medium": "sound", "exports": ["mp3", "wav", "broadcast", "stems"]});
        assert_eq!(
            exports_words(&sting).unwrap(),
            "WAV and MP3 at −16 LUFS for the web, a −23 LUFS broadcast master, and stems"
        );
        let pack = json!({"medium": "sound", "form": "sfx-pack", "count": 4, "exports": ["wav", "mp3", "zip"]});
        assert_eq!(
            exports_words(&pack).unwrap(),
            "Each sound as WAV and MP3, and a ZIP of the pack"
        );
        let poster = json!({"medium": "image", "exports": ["pdf", "png", "jpg", "webp"]});
        assert_eq!(
            exports_words(&poster).unwrap(),
            "A print-ready PDF, PNG, JPG and WebP"
        );
        let read = json!({"medium": "sound", "voice": {"key": "warm-narrator", "name": "Warm narrator"}, "from": "revision"});
        assert_eq!(
            plan_lines(&read),
            [
                "   Voice: Warm narrator.",
                "   From a made piece: it, changed as asked."
            ]
        );
        assert!(plan_delivery(&json!({"medium": "video"})).is_none());
        assert_eq!(plan_delivery(&set).unwrap()["form_label"], "Social set");
    }

    #[test]
    fn a_made_pieces_files_come_most_useful_first() {
        let mut files = Vec::new();
        for size in [
            "instagram-post",
            "instagram-story",
            "linkedin-post",
            "x-post",
            "link-preview",
            "ad-square",
        ] {
            for format in ["png", "jpg", "webp"] {
                files.push(json!({"id": format!("0-{size}.{format}"), "label": format!("{size} · {format}"), "format": format}));
            }
        }
        files.push(json!({"id": "0-set.zip", "label": "All 6 sizes · ZIP", "format": "zip"}));
        let (chosen, rest) = chosen_files(&files, "image", FILES_PER_PIECE);
        assert_eq!(chosen.len(), 12);
        assert_eq!(rest.len(), 7);
        let ids: Vec<&str> = chosen.iter().map(|f| f["id"].as_str().unwrap()).collect();
        assert_eq!(ids[0], "0-instagram-post.png");
        assert_eq!(ids[5], "0-ad-square.png");
        assert_eq!(ids[6], "0-set.zip");
        assert_eq!(
            more_files_line(&rest, &files).unwrap(),
            "   And 7 more files (JPG and WebP), all in the ZIP."
        );

        let sound = vec![
            json!({"id": "1-web.wav", "format": "wav", "role": "web"}),
            json!({"id": "1-broadcast.mp3", "format": "mp3", "role": "broadcast"}),
            json!({"id": "1-stems.zip", "format": "zip", "role": "stem"}),
            json!({"id": "1-broadcast.wav", "format": "wav", "role": "broadcast"}),
            json!({"id": "1-web.mp3", "format": "mp3", "role": "web"}),
        ];
        let (chosen, rest) = chosen_files(&sound, "sound", FILES_PER_PIECE);
        let ids: Vec<&str> = chosen.iter().map(|f| f["id"].as_str().unwrap()).collect();
        assert_eq!(
            ids,
            [
                "1-web.mp3",
                "1-broadcast.wav",
                "1-stems.zip",
                "1-web.wav",
                "1-broadcast.mp3"
            ]
        );
        assert!(rest.is_empty());
        assert!(is_main_download(&sound[3], "sound"));
        assert!(!is_main_download(&sound[0], "sound"));

        let line = file_line(
            &json!({"label": "Instagram post · PNG", "width": 1080, "height": 1350, "sizeBytes": 1_258_291}),
            Some(&SignedFile {
                url: "https://f.example/a".into(),
                expires_in_seconds: Some(900),
                size_bytes: None,
            }),
        );
        assert_eq!(
            line,
            "   - Instagram post · PNG · 1080×1350 · 1.2 MB: https://f.example/a"
        );
        assert_eq!(bytes_words(2000), "2 KB");
    }

    #[test]
    fn checks_say_what_did_not_pass() {
        let sizes = json!([{"id": "instagram-story", "label": "Instagram story"}]);
        let checks = vec![
            json!({"name": "Mark contrast", "ok": true}),
            json!({"name": "Type inside the safe area", "ok": false, "detail": "the headline runs under the reply bar.", "size": "instagram-story"}),
            json!({"name": "Instagram story", "ok": false, "detail": "could not be made", "size": "instagram-story"}),
        ];
        assert_eq!(
            checks_line(&checks, Some(&sizes)).unwrap(),
            "   Checks: 1 of 3 passed. Not passed: Type inside the safe area (Instagram story): the headline runs under the reply bar; Instagram story: could not be made."
        );
        assert_eq!(
            checks_line(&checks[..1], None).unwrap(),
            "   Checks: the one check passed."
        );
        assert!(checks_line(&[], None).is_none());
        assert_eq!(
            pack_words(Some(
                &json!([{"title": "Tap", "seconds": 0.4}, {"title": "Success", "seconds": 1}])
            ))
            .unwrap(),
            "Tap 0.4 s and Success 1 s"
        );
    }
}
