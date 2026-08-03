//! Parses the private RSC club payload used because no JSON endpoint names courts.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub(super) struct ResourceDto {
    #[serde(rename = "resourceId")]
    pub(super) resource_id: Uuid,
    pub(super) name: String,
    pub(super) sport: String,
    #[serde(default)]
    pub(super) features: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct OpeningHoursDto {
    pub(super) opening_time: String,
}

#[derive(Debug)]
pub(super) struct ClubPage {
    pub(super) tenant_id: Uuid,
    pub(super) timezone: String,
    pub(super) resources: Vec<ResourceDto>,
    pub(super) opening_hours: HashMap<String, OpeningHoursDto>,
}

impl ClubPage {
    pub(super) fn parse(body: &str) -> Result<Self> {
        // Keys such as timezone also occur outside the tenant object.
        let tenant_at = body
            .find("\"tenant_id\":")
            .context("club page has no tenant object; the payload shape changed")?;

        let tenant_id: Uuid = serde_json::from_str(value_after_key(body, "tenant_id", tenant_at)?)
            .context("parsing the club page's tenant_id")?;
        let timezone: String = serde_json::from_str(value_after_key(body, "timezone", tenant_at)?)
            .context("parsing the club page's timezone")?;
        let resources: Vec<ResourceDto> =
            serde_json::from_str(value_after_key(body, "resources", tenant_at)?)
                .context("parsing the club page's resources")?;
        let opening_hours: HashMap<String, OpeningHoursDto> =
            serde_json::from_str(value_after_key(body, "opening_hours", tenant_at)?)
                .context("parsing the club page's opening_hours")?;

        Ok(Self {
            tenant_id,
            timezone,
            resources,
            opening_hours,
        })
    }
}

fn value_after_key<'a>(body: &'a str, key: &str, from: usize) -> Result<&'a str> {
    let needle = format!("\"{key}\":");
    let at = body[from..]
        .find(&needle)
        .with_context(|| format!("club page has no {needle:?}; the payload shape changed"))?
        + from
        + needle.len();
    slice_value(body, at).with_context(|| format!("reading the value of {needle:?}"))
}

fn slice_value(body: &str, start: usize) -> Result<&str> {
    // JSON delimiters are ASCII, so returned offsets remain UTF-8 boundaries.
    let bytes = body.as_bytes();
    match bytes.get(start) {
        Some(b'[') | Some(b'{') => {}
        Some(b'"') => return slice_string(body, start),
        Some(other) => bail!(
            "expected a JSON array, object or string, found {:?}",
            *other as char
        ),
        None => bail!("value is past the end of the payload"),
    }

    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in bytes[start..].iter().enumerate() {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'"' {
            in_string = !in_string;
        } else if !in_string {
            match byte {
                b'[' | b'{' => depth += 1,
                b']' | b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(&body[start..start + offset + 1]);
                    }
                }
                _ => {}
            }
        }
    }
    bail!("unbalanced JSON: the value is never closed")
}

fn slice_string(body: &str, start: usize) -> Result<&str> {
    let bytes = body.as_bytes();
    let mut escaped = false;
    for (offset, byte) in bytes[start + 1..].iter().enumerate() {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'"' {
            return Ok(&body[start..start + offset + 2]);
        }
    }
    bail!("unterminated JSON string")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLUB_PAGE: &str = include_str!("testdata/club.rsc");

    #[test]
    fn parses_a_real_club_page() {
        let page = ClubPage::parse(CLUB_PAGE).expect("parse");

        assert_eq!(
            page.tenant_id,
            Uuid::parse_str("f8483f72-1d14-49eb-a98b-e4b89d969c78").unwrap()
        );
        assert_eq!(page.timezone, "Europe/Berlin");
        assert_eq!(page.resources.len(), 9);
        assert_eq!(page.resources[0].name, "Court 1 (Indoor)");
        assert_eq!(page.resources[0].sport, "PADEL");
        assert_eq!(
            page.resources[0].features,
            vec!["indoor", "double", "crystal"]
        );
        assert_eq!(page.opening_hours["MONDAY"].opening_time, "07:00");
    }

    #[test]
    fn a_nested_array_is_not_truncated() {
        let body = r#"junk"resources":[{"resourceId":"x","name":"a","features":["indoor","double"]}],"next":1"#;
        assert_eq!(
            value_after_key(body, "resources", 0).unwrap(),
            r#"[{"resourceId":"x","name":"a","features":["indoor","double"]}]"#
        );
    }

    #[test]
    fn structural_characters_inside_strings_are_ignored() {
        let body = r#""resources":[{"name":"Court [1] {\"Centre\"}"}]tail"#;
        assert_eq!(
            value_after_key(body, "resources", 0).unwrap(),
            r#"[{"name":"Court [1] {\"Centre\"}"}]"#
        );
    }

    #[test]
    fn the_club_timezone_is_read_from_the_tenant_not_an_earlier_block() {
        let decoy = format!(
            r#"{{"locale":{{"timezone":"America/New_York"}}}}{}"#,
            CLUB_PAGE
        );

        let page = ClubPage::parse(&decoy).expect("parse");

        assert_eq!(page.timezone, "Europe/Berlin");
    }

    #[test]
    fn a_payload_without_a_tenant_object_is_rejected() {
        let error = ClubPage::parse(r#"{"locale":{"timezone":"Europe/Berlin"}}"#).unwrap_err();
        assert!(
            format!("{error:#}").contains("tenant"),
            "unhelpful error: {error:#}"
        );
    }

    #[test]
    fn a_bare_string_value_is_read_whole() {
        let body = r#"x"timezone":"Europe/Berlin","next":1"#;
        assert_eq!(
            value_after_key(body, "timezone", 0).unwrap(),
            "\"Europe/Berlin\""
        );
    }

    #[test]
    fn multibyte_names_do_not_split_the_value() {
        let body = r#""resources":[{"name":"Platz München – Süd"}]tail"#;
        assert_eq!(
            value_after_key(body, "resources", 0).unwrap(),
            r#"[{"name":"Platz München – Süd"}]"#
        );
    }

    #[test]
    fn a_missing_key_names_itself() {
        let error = value_after_key(r#"{"other":1}"#, "resources", 0).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("resources"), "unhelpful error: {message}");
        assert!(
            message.contains("payload shape changed"),
            "does not point at the cause: {message}"
        );
    }

    #[test]
    fn an_unbalanced_value_is_rejected() {
        assert!(value_after_key(r#""resources":[{"name":"a"}"#, "resources", 0).is_err());
        assert!(value_after_key(r#""timezone":"Europe/Berlin"#, "timezone", 0).is_err());
    }

    #[test]
    fn a_non_json_value_is_rejected() {
        assert!(value_after_key(r#""resources":42"#, "resources", 0).is_err());
    }
}
