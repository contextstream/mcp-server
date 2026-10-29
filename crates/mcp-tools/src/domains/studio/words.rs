//! A making in plain words: its pieces, where it stands, and what went wrong.
//!
//! The same readings as ContextStudio's web studio (contextstudio
//! `packages/web/src/lib/make.ts`), so a person reads a making the same way in
//! the studio and through their agent. Pure functions over Create's answers.

use serde_json::Value;

use super::forms::form_label;

/// A piece's medium in words.
pub(crate) fn medium_name(medium: &str) -> &'static str {
    match medium {
        "image" => "Image",
        "video" => "Video",
        "sound" => "Sound",
        _ => "Piece",
    }
}

/// "Wide 16:9", "Tall 9:16".
pub(crate) fn shape_words(aspect: Option<&str>) -> Option<String> {
    let aspect = aspect?.trim();
    if aspect.is_empty() {
        return None;
    }
    let name = match aspect {
        "16:9" => "Wide",
        "9:16" => "Tall",
        "1:1" => "Square",
        "4:5" | "3:4" | "2:3" => "Portrait",
        "3:2" | "4:3" => "Landscape",
        "21:9" => "Cinema wide",
        _ => return Some(aspect.to_string()),
    };
    Some(format!("{name} {aspect}"))
}

fn number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value:.1}")
    }
}

/// "8 seconds", "20 seconds, at most 60".
pub(crate) fn length_words(seconds: Option<f64>, max_seconds: Option<f64>) -> Option<String> {
    let seconds = seconds?;
    let words = format!(
        "{} {}",
        number(seconds),
        if seconds == 1.0 { "second" } else { "seconds" }
    );
    Some(match max_seconds {
        Some(max) if max > seconds => format!("{words}, at most {}", number(max)),
        _ => words,
    })
}

/// "Video · Tall 9:16 · 8 seconds"; brand work also names its form and how many sizes or
/// sounds: "Image · Social set · 3 sizes", "Sound · Sound effects pack · 4 sounds · 1.5
/// seconds each".
pub(crate) fn piece_facts(piece: &Value) -> String {
    let medium = medium_name(piece.get("medium").and_then(Value::as_str).unwrap_or(""));
    let mut parts = vec![medium.to_string()];
    if let Some(form) = piece
        .get("form")
        .and_then(Value::as_str)
        .filter(|f| !f.trim().is_empty())
    {
        parts.push(form_label(form));
    }
    let sizes = piece.get("sizes").and_then(Value::as_array);
    match sizes.map(Vec::len).unwrap_or(0) {
        n if n > 1 => parts.push(format!("{n} sizes")),
        _ => {
            let aspect = piece.get("aspect").and_then(Value::as_str).or_else(|| {
                sizes
                    .and_then(|s| s.first())
                    .and_then(|s| s.get("aspect"))
                    .and_then(Value::as_str)
            });
            if let Some(shape) = shape_words(aspect) {
                parts.push(shape);
            }
        }
    }
    let count = piece
        .get("count")
        .and_then(Value::as_u64)
        .filter(|c| *c > 1);
    if let Some(count) = count {
        parts.push(format!("{count} sounds"));
    }
    if let Some(length) = length_words(
        piece.get("seconds").and_then(Value::as_f64),
        piece.get("maxSeconds").and_then(Value::as_f64),
    ) {
        parts.push(if count.is_some() {
            format!("{length} each")
        } else {
            length
        });
    }
    parts.join(" · ")
}

/// Whether a piece is a product explainer, filmed from a connected product.
pub(crate) fn is_explainer(piece: &Value) -> bool {
    piece.get("kindOfWork").and_then(Value::as_str) == Some("video.explainer")
}

/// The producer's stage for a making as a whole, in words.
pub(crate) fn stage_words(stage: Option<&str>) -> Option<String> {
    let text = stage?.trim();
    if text.is_empty() {
        return None;
    }
    let lower = text.to_lowercase();
    let whole = match lower.as_str() {
        "reading the brief" => Some("Reading the brief"),
        "planning" | "pricing" | "planned" | "making" => Some("Starting"),
        "done" | "partial" => Some("Finishing"),
        "failed" => Some("Stopping"),
        "over the cap" => Some("Stopped at the price it was given"),
        _ => None,
    };
    if let Some(words) = whole {
        return Some(words.to_string());
    }
    let mut words = lower.split_whitespace();
    let first = words.next().unwrap_or("");
    let rest: Vec<&str> = words.collect();
    let reading = match (first, rest.as_slice()) {
        ("writing", ["the", "story", "of", _]) => "Writing the story from your product’s map",
        ("filming", [_, count]) if count.starts_with('(') => "Filming your product",
        ("recording", ["the", "narration", "of", _]) => "Recording the narration",
        ("writing", [_]) => "Writing the takes",
        ("making", [_, "at", _]) => "Making the chosen take at full size",
        ("making", [_]) => "Making takes",
        ("judging", [_]) => "Judging the takes",
        ("re-directing", [_]) => "Taking it in a new direction",
        ("mixing", [_]) => "Mixing and mastering",
        ("drawing", [_]) => "Drawing it",
        ("cutting", [_]) => "Cutting the film",
        ("scoring", [_, "to", "the", "cut"]) => "Scoring the cut",
        _ => "",
    };
    if !reading.is_empty() {
        return Some(reading.to_string());
    }
    let mut chars = text.chars();
    Some(match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    })
}

/// Why a plan couldn't be made, by its code.
pub(crate) fn quote_problem(code: Option<&str>) -> &'static str {
    match code {
        Some("make_unplanned") => "This couldn’t be planned from what was asked. Say what it’s for and who will see it, then plan it again.",
        Some("brand_unavailable") => "Everything is made from the team’s brand, and this workspace has none yet. Add one in ContextStudio (Brand), then plan it again.",
        Some("reference_unavailable") => "One of the attached images can’t be used. Leave it out, or attach it again.",
        Some("media_unavailable") => "Part of this can’t be made right now: the provider isn’t taking that kind of media. Plan it again later, or ask for something else.",
        Some("model_unavailable") => "The studio isn’t taking new work for a few minutes. Try again shortly.",
        Some("engine_unavailable") => "The studio couldn’t plan this just now. Try again in a moment.",
        Some("timed_out") => "Planning took too long and was stopped. Plan it again.",
        Some("invalid_request") => "The studio couldn’t read this request. Change the wording, then plan it again.",
        _ => "This couldn’t be planned. Try again.",
    }
}

/// Why a piece wasn't made, by its code; the medium names what the provider refused.
pub(crate) fn piece_problem(code: Option<&str>, medium: &str) -> String {
    match code {
        Some("media_unavailable") => format!(
            "The {} provider isn’t taking new work right now, so this piece wasn’t made. It isn’t charged.",
            medium_name(medium).to_lowercase()
        ),
        Some("reference_unavailable") => {
            "An attached image couldn’t be read, so this piece wasn’t made. It isn’t charged."
                .to_string()
        }
        Some("timed_out") => "This piece took too long and was stopped. It isn’t charged.".to_string(),
        _ => "This piece couldn’t be finished. It isn’t charged.".to_string(),
    }
}

/// Where a piece stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PieceState {
    Waiting,
    Working,
    Made,
    Failed,
    Stopped,
}

impl PieceState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Working => "working",
            Self::Made => "made",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

/// One planned piece and how it stands now.
#[derive(Debug, Clone)]
pub(crate) struct PieceProgress {
    pub piece: Value,
    pub state: PieceState,
    pub words: String,
    pub output: Option<Value>,
    pub made: Option<Value>,
    pub failed: Option<Value>,
}

impl PieceProgress {
    pub(crate) fn index(&self) -> i64 {
        self.piece.get("index").and_then(Value::as_i64).unwrap_or(0)
    }

    pub(crate) fn title(&self) -> &str {
        self.piece
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Untitled piece")
    }
}

const TERMINAL: &[&str] = &["succeeded", "failed", "cancelled"];

/// Whether a job has ended.
pub(crate) fn job_ended(job: &Value) -> bool {
    TERMINAL.contains(&job.get("status").and_then(Value::as_str).unwrap_or(""))
}

fn by_index(list: Option<&Value>, index: i64) -> Option<&Value> {
    list?
        .as_array()?
        .iter()
        .find(|item| item.get("index").and_then(Value::as_i64) == Some(index))
}

/// Each planned piece of a making job, and how it stands, by its own status.
pub(crate) fn piece_progress(job: &Value) -> Vec<PieceProgress> {
    let make = job.get("make");
    let pieces = make
        .and_then(|m| m.pointer("/plan/pieces"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let discarded: Vec<i64> = job
        .get("discardedIndexes")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let status = job.get("status").and_then(Value::as_str).unwrap_or("");
    let ended = job_ended(job);
    let job_code = job.pointer("/error/code").and_then(Value::as_str);
    pieces
        .into_iter()
        .map(|piece| {
            let index = piece.get("index").and_then(Value::as_i64).unwrap_or(0);
            let medium = piece
                .get("medium")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let output = by_index(job.get("outputs"), index)
                .filter(|_| !discarded.contains(&index))
                .cloned();
            let made = by_index(make.and_then(|m| m.get("pieces")), index).cloned();
            let failed = by_index(make.and_then(|m| m.get("failed")), index).cloned();
            let own = piece
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string);
            let progress = |state: PieceState, words: String| PieceProgress {
                piece: piece.clone(),
                state,
                words,
                output: output.clone(),
                made: made.clone(),
                failed: failed.clone(),
            };
            if output.is_some() {
                return progress(PieceState::Made, "Made".to_string());
            }
            let own = own.unwrap_or_else(|| {
                if made.is_some() {
                    "made"
                } else if failed.is_some() {
                    "failed"
                } else {
                    match status {
                        "cancelled" => "stopped",
                        "failed" => "failed",
                        "queued" => "planned",
                        _ => "making",
                    }
                }
                .to_string()
            });
            match own.as_str() {
                "made" => progress(
                    PieceState::Made,
                    if made.is_some() { "Made" } else { "Deleted" }.to_string(),
                ),
                _ if own == "failed" || failed.is_some() => {
                    let code = failed
                        .as_ref()
                        .and_then(|f| f.get("code"))
                        .and_then(Value::as_str)
                        .or(job_code);
                    let medium = failed
                        .as_ref()
                        .and_then(|f| f.get("medium"))
                        .and_then(Value::as_str)
                        .unwrap_or(&medium);
                    progress(PieceState::Failed, piece_problem(code, medium))
                }
                "stopped" => progress(
                    PieceState::Stopped,
                    "Stopped before it was made".to_string(),
                ),
                _ if ended => progress(PieceState::Stopped, "Not made".to_string()),
                "planned" => progress(PieceState::Waiting, "Waiting to start".to_string()),
                _ if status == "queued" => {
                    progress(PieceState::Waiting, "Waiting to start".to_string())
                }
                _ if is_explainer(&piece) => {
                    let words = piece
                        .get("film")
                        .map(film_phase_words)
                        .or_else(|| {
                            stage_words(make.and_then(|m| m.get("stage")).and_then(Value::as_str))
                        })
                        .unwrap_or_else(|| "Filming your product".to_string());
                    progress(PieceState::Working, words)
                }
                "making" => progress(PieceState::Working, "Making takes".to_string()),
                "judging" => progress(PieceState::Working, "Judging the takes".to_string()),
                "composing" => progress(PieceState::Working, "Putting it together".to_string()),
                _ => progress(PieceState::Working, "In the works".to_string()),
            }
        })
        .collect()
}

/// One line for where a making is.
pub(crate) fn headline(job: &Value) -> String {
    let pieces = piece_progress(job);
    let made = pieces
        .iter()
        .filter(|p| p.state == PieceState::Made)
        .count();
    let count = pieces.len();
    match job.get("status").and_then(Value::as_str).unwrap_or("") {
        "queued" => "Waiting to start".to_string(),
        "succeeded" => {
            if count == 1 {
                "Made".to_string()
            } else {
                format!("All {count} pieces made")
            }
        }
        "cancelled" if made > 0 => format!("Stopped with {made} of {count} made"),
        "cancelled" => "Stopped".to_string(),
        "failed" if made > 0 => format!("{made} of {count} made"),
        "failed" => "It couldn’t be made".to_string(),
        status => {
            let stage = job.pointer("/make/stage").and_then(Value::as_str);
            let explainer = pieces.iter().any(|p| is_explainer(&p.piece));
            if explainer && status != "processing" {
                return stage_words(stage).unwrap_or_else(|| "Starting".to_string());
            }
            let busy = pieces
                .iter()
                .find(|p| p.piece.get("status").and_then(Value::as_str) == Some("judging"))
                .or_else(|| {
                    pieces.iter().find(|p| {
                        p.piece.get("status").and_then(Value::as_str) == Some("composing")
                    })
                });
            if let Some(busy) = busy {
                return format!("{}: {}", busy.words, busy.title());
            }
            if status == "processing" {
                return "Putting the pieces in the library".to_string();
            }
            stage_words(stage).unwrap_or_else(|| "Starting".to_string())
        }
    }
}

/// Why a making ended without all its pieces, or `None`.
pub(crate) fn making_problem(job: &Value) -> Option<String> {
    if job.get("status").and_then(Value::as_str) != Some("failed") {
        return None;
    }
    let job_code = job.pointer("/error/code").and_then(Value::as_str);
    let failed: Vec<PieceProgress> = piece_progress(job)
        .into_iter()
        .filter(|p| p.state == PieceState::Failed)
        .collect();
    let codes: Vec<&str> = {
        let mut codes: Vec<&str> = failed
            .iter()
            .map(|p| {
                p.failed
                    .as_ref()
                    .and_then(|f| f.get("code"))
                    .and_then(Value::as_str)
                    .or(job_code)
                    .unwrap_or("engine_unavailable")
            })
            .collect();
        codes.dedup();
        codes
    };
    let charged = "Only what was delivered is charged.";
    if codes.len() > 1 {
        return Some(format!(
            "{} pieces couldn’t be made, each for the reason it shows. {charged}",
            failed.len()
        ));
    }
    Some(match codes.first().copied().or(job_code) {
        Some("media_unavailable") => format!(
            "The provider for that media isn’t taking new work right now, so {} made. {charged}",
            if failed.len() == 1 {
                "that piece wasn’t"
            } else {
                "those pieces weren’t"
            }
        ),
        Some("timed_out") => format!("This took too long and was stopped. {charged}"),
        Some("make_unplanned") => {
            "The plan no longer fit what the studio can make. Plan it again.".to_string()
        }
        Some("reference_unavailable") => {
            "An attached image couldn’t be read. Attach it again and plan it again.".to_string()
        }
        Some("brand_unavailable") => {
            "The team’s brand couldn’t be read. Check it in ContextStudio, then plan it again."
                .to_string()
        }
        _ => format!(
            "The studio had a problem finishing {}. {charged}",
            if failed.len() == 1 { "a piece" } else { "this" }
        ),
    })
}

/// "Northwind Orders, map v2".
fn film_source(film: &Value) -> String {
    let name = film
        .pointer("/product/name")
        .and_then(Value::as_str)
        .unwrap_or("your product");
    match film.pointer("/product/mapVersion").and_then(Value::as_i64) {
        Some(version) => format!("{name}, map v{version}"),
        None => name.to_string(),
    }
}

/// Where a product explainer's film is, in one sentence.
pub(crate) fn film_phase_words(film: &Value) -> String {
    let beats = film
        .get("beats")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let count = beats.len();
    let status_of = |beat: &Value| {
        beat.get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let filmed = beats.iter().filter(|b| status_of(b) == "filmed").count();
    match film.get("phase").and_then(Value::as_str) {
        Some("story") => format!("Writing the story from {}.", film_source(film)),
        Some("filming") => {
            let busy = beats.iter().position(|b| {
                matches!(status_of(b).as_str(), "filming" | "refilming" | "replanned")
            });
            match busy {
                None => format!("Filming {count} beats: {filmed} of {count} filmed."),
                Some(at) => {
                    let doing = match status_of(&beats[at]).as_str() {
                        "refilming" => format!("Filming beat {} of {count} again", at + 1),
                        "replanned" => format!("Writing beat {} of {count} again", at + 1),
                        _ => format!("Filming beat {} of {count}", at + 1),
                    };
                    format!("{doing}: {filmed} of {count} filmed.")
                }
            }
        }
        Some("narration") => format!("All {count} beats filmed. Recording the narration."),
        Some("cutting") => "Narrated. Cutting the film in the house look.".to_string(),
        Some("done") => format!("Filmed in {count} beats from {}.", film_source(film)),
        _ => "Waiting to start.".to_string(),
    }
}

/// An explainer's beats, one line each: "1. Filter the records to decisions (Records)".
pub(crate) fn beat_lines(film: &Value) -> Vec<String> {
    film.get("beats")
        .and_then(Value::as_array)
        .map(|beats| {
            beats
                .iter()
                .enumerate()
                .filter_map(|(at, beat)| {
                    let shows = beat.get("shows").and_then(Value::as_str)?.trim();
                    let screen = beat
                        .get("screen")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty());
                    let status = beat
                        .get("status")
                        .and_then(Value::as_str)
                        .filter(|s| *s != "filmed" && *s != "waiting");
                    let mut line = format!("{}. {shows}", at + 1);
                    if let Some(screen) = screen {
                        line.push_str(&format!(" ({screen})"));
                    }
                    if let Some(status) = status {
                        line.push_str(&format!(" [{status}]"));
                    }
                    Some(line)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A sentence with its first letter up and a full stop, or `None` when empty.
pub(crate) fn sentence(text: Option<&str>) -> Option<String> {
    let flat = text?.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return None;
    }
    let mut chars = flat.chars();
    let first = chars.next()?.to_uppercase().collect::<String>();
    let mut out = first + chars.as_str();
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    Some(out)
}

/// Text cut to `max` characters at a word, with an ellipsis when cut.
pub(crate) fn clip(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    let cut = match cut.rfind(' ') {
        Some(space) if space > max / 2 => cut[..space].to_string(),
        _ => cut,
    };
    format!("{cut}…")
}

/// "1,234".
pub(crate) fn credits(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::new();
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    if value < 0 {
        format!("-{out}")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pieces_read_in_words() {
        assert_eq!(
            piece_facts(&json!({"medium": "video", "aspect": "9:16", "seconds": 8})),
            "Video · Tall 9:16 · 8 seconds"
        );
        assert_eq!(
            piece_facts(
                &json!({"medium": "video", "aspect": "16:9", "seconds": 20, "maxSeconds": 60})
            ),
            "Video · Wide 16:9 · 20 seconds, at most 60"
        );
        assert_eq!(
            piece_facts(&json!({"medium": "sound", "seconds": 1.5})),
            "Sound · 1.5 seconds"
        );
        assert_eq!(
            piece_facts(
                &json!({"medium": "image", "form": "social-set", "aspect": "4:5",
                "sizes": [{"id": "instagram-post"}, {"id": "instagram-story"}, {"id": "linkedin-post"}]})
            ),
            "Image · Social set · 3 sizes"
        );
        assert_eq!(
            piece_facts(&json!({"medium": "image", "form": "hero",
                "sizes": [{"id": "web-hero", "aspect": "16:9"}]})),
            "Image · Hero image · Wide 16:9"
        );
        assert_eq!(
            piece_facts(
                &json!({"medium": "sound", "form": "sfx-pack", "count": 4, "seconds": 1.5})
            ),
            "Sound · Sound effects pack · 4 sounds · 1.5 seconds each"
        );
        assert_eq!(credits(1234567), "1,234,567");
        assert_eq!(credits(950), "950");
        assert_eq!(clip("one two three four five", 12), "one two…");
        assert_eq!(
            sentence(Some("a quiet sting")),
            Some("A quiet sting.".to_string())
        );
        assert_eq!(sentence(Some("  ")), None);
    }

    #[test]
    fn stages_read_like_the_studio() {
        assert_eq!(
            stage_words(Some("judging hero-still")).unwrap(),
            "Judging the takes"
        );
        assert_eq!(
            stage_words(Some("making hero at 2k")).unwrap(),
            "Making the chosen take at full size"
        );
        assert_eq!(
            stage_words(Some("filming film (2)")).unwrap(),
            "Filming your product"
        );
        assert_eq!(
            stage_words(Some("scoring film to the cut")).unwrap(),
            "Scoring the cut"
        );
        assert_eq!(stage_words(Some("planning")).unwrap(), "Starting");
        assert_eq!(stage_words(Some("something new")).unwrap(), "Something new");
        assert_eq!(stage_words(Some(" ")), None);
    }

    fn job(status: &str, pieces: Value, outputs: Value, failed: Value) -> Value {
        json!({
            "status": status,
            "outputs": outputs,
            "make": {"plan": {"pieces": pieces}, "pieces": [], "failed": failed, "stage": "judging sting"},
        })
    }

    #[test]
    fn each_piece_says_where_it_stands() {
        let pieces = json!([
            {"index": 0, "id": "sting", "title": "Sting", "medium": "sound", "status": "judging"},
            {"index": 1, "id": "board", "title": "Board", "medium": "image", "status": "made"},
            {"index": 2, "id": "film", "title": "Film", "medium": "video", "status": "planned"},
        ]);
        let running = job(
            "generating",
            pieces.clone(),
            json!([{"index": 1, "contentId": "c", "mimeType": "image/png"}]),
            json!([]),
        );
        let progress = piece_progress(&running);
        assert_eq!(progress[0].state, PieceState::Working);
        assert_eq!(progress[0].words, "Judging the takes");
        assert_eq!(progress[1].state, PieceState::Made);
        assert_eq!(progress[2].state, PieceState::Waiting);
        assert_eq!(headline(&running), "Judging the takes: Sting");

        let failed = job(
            "failed",
            json!([{"index": 0, "title": "Board", "medium": "image", "status": "failed"}]),
            json!([]),
            json!([{"index": 0, "code": "media_unavailable", "medium": "image"}]),
        );
        let progress = piece_progress(&failed);
        assert_eq!(progress[0].state, PieceState::Failed);
        assert!(progress[0].words.contains("image provider"));
        assert_eq!(headline(&failed), "It couldn’t be made");
        assert!(making_problem(&failed)
            .unwrap()
            .contains("that piece wasn’t made"));
        assert!(making_problem(&running).is_none());
    }

    #[test]
    fn an_explainer_says_where_its_film_is() {
        let film = json!({
            "product": {"name": "Offscript Studio", "mapVersion": 8},
            "phase": "filming",
            "beats": [
                {"shows": "Open the records", "screen": "Records", "status": "filmed"},
                {"shows": "Filter to decisions", "screen": "Records", "status": "filming"},
                {"shows": "Read one", "screen": "Records", "status": "waiting"},
            ],
        });
        assert_eq!(
            film_phase_words(&film),
            "Filming beat 2 of 3: 1 of 3 filmed."
        );
        assert_eq!(
            beat_lines(&film),
            [
                "1. Open the records (Records)",
                "2. Filter to decisions (Records) [filming]",
                "3. Read one (Records)"
            ]
        );
        let done = json!({"product": {"name": "Offscript Studio", "mapVersion": 8}, "phase": "done", "beats": [{}, {}]});
        assert_eq!(
            film_phase_words(&done),
            "Filmed in 2 beats from Offscript Studio, map v8."
        );
    }
}
