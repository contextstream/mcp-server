//! Readable coach references and keeping internal ids out of replies.
//!
//! | Kind     | Format          | Example            |
//! | -------- | --------------- | ------------------ |
//! | workflow | `slug`          | `weekly-note`      |
//! | job      | `slug#N`        | `weekly-note#4`    |
//! | output   | `slug#N/vK`     | `weekly-note#4/v2` |
//! | feedback | `slug#N/fK`     | `weekly-note#4/f1` |
//! | guidance | `slug/gK`       | `weekly-note/g3`   |
//!
//! The tools take and show only these. A UUID is refused as input, and any
//! UUID-shaped text is masked before a reply leaves the tool.

use mcp_types::{Error, Result};

/// What a reference names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefKind {
    Workflow,
    Job,
    Output,
    Feedback,
    Guidance,
}

impl RefKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Workflow => "workflow",
            Self::Job => "job",
            Self::Output => "output",
            Self::Feedback => "feedback",
            Self::Guidance => "guidance",
        }
    }

    fn example(self) -> &'static str {
        match self {
            Self::Workflow => "weekly-note",
            Self::Job => "weekly-note#4",
            Self::Output => "weekly-note#4/v2",
            Self::Feedback => "weekly-note#4/f1",
            Self::Guidance => "weekly-note/g3",
        }
    }
}

/// A parsed readable reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CoachRef {
    pub kind: RefKind,
    pub slug: String,
    pub job: Option<u32>,
    pub number: Option<u32>,
}

impl CoachRef {
    /// The workflow it belongs to.
    pub(crate) fn workflow(&self) -> String {
        self.slug.clone()
    }

    /// The job it belongs to, for job, output and feedback references.
    pub(crate) fn job_ref(&self) -> Option<String> {
        self.job.map(|job| format!("{}#{job}", self.slug))
    }
}

impl std::fmt::Display for CoachRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.kind, self.job, self.number) {
            (RefKind::Workflow, _, _) => write!(f, "{}", self.slug),
            (RefKind::Job, Some(job), _) => write!(f, "{}#{job}", self.slug),
            (RefKind::Output, Some(job), Some(n)) => write!(f, "{}#{job}/v{n}", self.slug),
            (RefKind::Feedback, Some(job), Some(n)) => write!(f, "{}#{job}/f{n}", self.slug),
            (RefKind::Guidance, _, Some(n)) => write!(f, "{}/g{n}", self.slug),
            _ => write!(f, "{}", self.slug),
        }
    }
}

fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && !slug.starts_with('-')
        && !slug.ends_with('-')
        && !slug.contains("--")
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn number(text: &str) -> Option<u32> {
    if text.is_empty() || text.len() > 6 || !text.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|value| *value >= 1)
}

/// Parse a readable reference. Slugs are compared lowercase.
pub(crate) fn parse(raw: &str) -> Option<CoachRef> {
    let raw = raw.trim().to_ascii_lowercase();
    if let Some((slug, rest)) = raw.split_once('#') {
        if !valid_slug(slug) {
            return None;
        }
        let (job, tail) = match rest.split_once('/') {
            Some((job, tail)) => (number(job)?, Some(tail)),
            None => (number(rest)?, None),
        };
        let slug = slug.to_string();
        return match tail {
            None => Some(CoachRef {
                kind: RefKind::Job,
                slug,
                job: Some(job),
                number: None,
            }),
            Some(tail) => {
                let (kind, digits) = match tail.split_at_checked(1)? {
                    ("v", digits) => (RefKind::Output, digits),
                    ("f", digits) => (RefKind::Feedback, digits),
                    _ => return None,
                };
                Some(CoachRef {
                    kind,
                    slug,
                    job: Some(job),
                    number: Some(number(digits)?),
                })
            }
        };
    }
    if let Some((slug, tail)) = raw.split_once('/') {
        let digits = tail.strip_prefix('g')?;
        if !valid_slug(slug) {
            return None;
        }
        return Some(CoachRef {
            kind: RefKind::Guidance,
            slug: slug.to_string(),
            job: None,
            number: Some(number(digits)?),
        });
    }
    valid_slug(&raw).then_some(CoachRef {
        kind: RefKind::Workflow,
        slug: raw,
        job: None,
        number: None,
    })
}

/// The reference in `value`, which must be one of `kinds`.
pub(crate) fn expect(value: Option<&str>, field: &str, kinds: &[RefKind]) -> Result<CoachRef> {
    let raw = value
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .ok_or_else(|| {
            Error::Validation(format!(
                "{field} is required: a readable reference such as {}",
                kinds
                    .iter()
                    .map(|kind| format!("`{}`", kind.example()))
                    .collect::<Vec<_>>()
                    .join(" or ")
            ))
        })?;
    if contains_uuid(raw) {
        return Err(Error::Validation(format!(
            "{field} takes a readable reference (for example `{}`), never an internal id",
            kinds[0].example()
        )));
    }
    let parsed = parse(raw).ok_or_else(|| {
        Error::Validation(format!(
            "{field} {raw:?} is not a coach reference; use {}",
            kinds
                .iter()
                .map(|kind| format!("a {} like `{}`", kind.as_str(), kind.example()))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    if !kinds.contains(&parsed.kind) {
        return Err(Error::Validation(format!(
            "{field} {raw:?} names a {}; it must name {}",
            parsed.kind.as_str(),
            kinds
                .iter()
                .map(|kind| format!("a {} (`{}`)", kind.as_str(), kind.example()))
                .collect::<Vec<_>>()
                .join(" or ")
        )));
    }
    Ok(parsed)
}

// ============================================================================
// Internal ids
// ============================================================================

const UUID_GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
const UUID_LEN: usize = 36;

fn uuid_at(bytes: &[u8], start: usize) -> bool {
    if start + UUID_LEN > bytes.len() {
        return false;
    }
    let mut index = start;
    for (group, width) in UUID_GROUPS.iter().enumerate() {
        if group > 0 {
            if bytes[index] != b'-' {
                return false;
            }
            index += 1;
        }
        if !bytes[index..index + width]
            .iter()
            .all(u8::is_ascii_hexdigit)
        {
            return false;
        }
        index += width;
    }
    let before_ok = start == 0 || !bytes[start - 1].is_ascii_hexdigit();
    let after_ok = index == bytes.len() || !bytes[index].is_ascii_hexdigit();
    before_ok && after_ok
}

/// Whether `text` holds anything shaped like a UUID.
pub(crate) fn contains_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    (0..bytes.len()).any(|start| uuid_at(bytes, start))
}

/// `text` with every UUID-shaped run replaced by `[id]`.
pub(crate) fn mask_uuids(text: &str) -> String {
    let bytes = text.as_bytes();
    if !contains_uuid(text) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    let mut copied_from = 0;
    while index < bytes.len() {
        if uuid_at(bytes, index) {
            out.push_str(&text[copied_from..index]);
            out.push_str("[id]");
            index += UUID_LEN;
            copied_from = index;
        } else {
            index += 1;
        }
    }
    out.push_str(&text[copied_from..]);
    out
}

/// A JSON value with every string's UUIDs masked.
pub(crate) fn mask_uuids_in(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(text) => Value::String(mask_uuids(text)),
        Value::Array(items) => Value::Array(items.iter().map(mask_uuids_in).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), mask_uuids_in(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reference_shape_parses_and_round_trips() {
        for (raw, kind) in [
            ("weekly-note", RefKind::Workflow),
            ("weekly-note#4", RefKind::Job),
            ("weekly-note#4/v2", RefKind::Output),
            ("weekly-note#4/f1", RefKind::Feedback),
            ("weekly-note/g3", RefKind::Guidance),
        ] {
            let parsed = parse(raw).unwrap_or_else(|| panic!("{raw} did not parse"));
            assert_eq!(parsed.kind, kind, "{raw}");
            assert_eq!(parsed.to_string(), raw);
            assert_eq!(parsed.workflow(), "weekly-note");
        }
        assert_eq!(
            parse(" Weekly-Note#4/V2 ")
                .map(|r| r.to_string())
                .as_deref(),
            Some("weekly-note#4/v2")
        );
        assert_eq!(
            parse("weekly-note#4/v2").unwrap().job_ref().as_deref(),
            Some("weekly-note#4")
        );
        for bad in [
            "",
            "-x",
            "a--b",
            "weekly note",
            "weekly-note#0",
            "weekly-note#x",
            "weekly-note#4/x2",
            "weekly-note#4/v",
            "weekly-note/v3",
            "weekly-note/g",
        ] {
            assert!(parse(bad).is_none(), "{bad} parsed");
        }
    }

    #[test]
    fn expect_names_the_kind_it_wanted_and_refuses_ids() {
        let error = expect(Some("weekly-note#4"), "output", &[RefKind::Output]).unwrap_err();
        assert!(error.to_string().contains("names a job"), "{error}");
        let error = expect(
            Some("11111111-1111-4111-8111-111111111111"),
            "workflow",
            &[RefKind::Workflow],
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("never an internal id"),
            "{error}"
        );
        let error = expect(None, "job", &[RefKind::Job]).unwrap_err();
        assert!(error.to_string().contains("weekly-note#4"), "{error}");
    }

    #[test]
    fn uuids_are_found_and_masked_in_text() {
        let text = "see 11111111-1111-4111-8111-111111111111 and /x/22222222-2222-4222-8222-222222222222/y";
        assert!(contains_uuid(text));
        assert_eq!(mask_uuids(text), "see [id] and /x/[id]/y");
        assert!(!contains_uuid("weekly-note#4/v2 mcp-0123456789abcdef"));
        // A longer hex run is not a UUID.
        assert!(!contains_uuid("a11111111-1111-4111-8111-111111111111"));
        let value = serde_json::json!({"a": ["x 33333333-3333-4333-8333-333333333333"]});
        assert_eq!(mask_uuids_in(&value)["a"][0], "x [id]");
    }
}
