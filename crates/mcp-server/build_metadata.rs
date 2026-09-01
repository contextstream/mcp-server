use chrono::{DateTime, Utc};

pub fn source_build_date(
    source_date_epoch: Option<&str>,
    release_guard: Option<&str>,
) -> Result<String, String> {
    let required = match release_guard {
        None => false,
        Some("1") => true,
        Some(value) => {
            return Err(format!(
                "CONTEXTSTREAM_RELEASE_BUILD must be 1 when set; got {value:?}"
            ));
        }
    };

    let raw_epoch = match source_date_epoch {
        Some(value) => value,
        None if !required => return Ok("unknown".to_string()),
        None => {
            return Err("release builds require deterministic SOURCE_DATE_EPOCH".to_string());
        }
    };
    if raw_epoch.is_empty() || !raw_epoch.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "SOURCE_DATE_EPOCH must be an unsigned ASCII base-10 integer; got {raw_epoch:?}"
        ));
    }
    let epoch = raw_epoch
        .parse::<i64>()
        .map_err(|_| "SOURCE_DATE_EPOCH is outside the supported range".to_string())?;
    let timestamp = DateTime::<Utc>::from_timestamp(epoch, 0)
        .ok_or_else(|| "SOURCE_DATE_EPOCH is outside the UTC range".to_string())?;
    Ok(timestamp.format("%Y-%m-%d").to_string())
}

#[cfg(test)]
mod tests {
    use super::source_build_date;

    #[test]
    fn formats_source_epoch_in_utc_across_midnight() {
        assert_eq!(
            source_build_date(Some("0"), Some("1")).unwrap(),
            "1970-01-01"
        );
        assert_eq!(
            source_build_date(Some("946684799"), Some("1")).unwrap(),
            "1999-12-31"
        );
        assert_eq!(
            source_build_date(Some("946684800"), Some("1")).unwrap(),
            "2000-01-01"
        );
    }

    #[test]
    fn local_build_without_epoch_is_honestly_deterministic() {
        assert_eq!(source_build_date(None, None).unwrap(), "unknown");
    }

    #[test]
    fn release_epoch_and_guard_fail_closed() {
        assert!(source_build_date(None, Some("1")).is_err());
        for invalid in ["", " ", "+1", "-1", "1.5", "9223372036854775808"] {
            assert!(
                source_build_date(Some(invalid), Some("1")).is_err(),
                "{invalid:?}"
            );
        }
        assert!(source_build_date(Some("1"), Some("true")).is_err());
    }
}
