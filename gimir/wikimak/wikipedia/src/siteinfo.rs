//! MediaWiki API siteinfo capture for portable archives.

use std::path::Path;

use reqwest::blocking::Client;
use serde_json::Value;

use crate::archive::{
    ArchiveWriter, Record, SiteInfoRecord, SiteInterwikiRecord, SiteMagicWordRecord,
    SiteNamespaceRecord, DEFAULT_FRAME_TARGET,
};
use crate::{Error, Result};

pub fn fetch_siteinfo_archive(
    client: &Client,
    api_url: &str,
    output: impl AsRef<Path>,
) -> Result<()> {
    let response = client
        .get(api_url)
        .query(&[
            ("action", "query"),
            ("meta", "siteinfo"),
            (
                "siprop",
                "general|namespaces|namespacealiases|interwikimap|magicwords",
            ),
            ("format", "json"),
            ("formatversion", "2"),
        ])
        .send()
        .map_err(wikimak_mediawiki::Error::from)?
        .error_for_status()
        .map_err(wikimak_mediawiki::Error::from)?;
    let bytes = response
        .bytes()
        .map_err(wikimak_mediawiki::Error::from)?;
    let root: Value =
        serde_json::from_slice(&bytes).map_err(|error| parse_error(error.to_string()))?;
    let query = root
        .get("query")
        .ok_or_else(|| parse_error("siteinfo response has no query object"))?;
    let general = query
        .get("general")
        .ok_or_else(|| parse_error("siteinfo response has no general object"))?;

    let aliases = query
        .get("namespacealiases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|alias| {
            Some((
                alias.get("id")?.as_i64()? as i32,
                alias.get("alias")?.as_str()?.to_string(),
            ))
        })
        .collect::<Vec<_>>();
    let mut namespaces = query
        .get("namespaces")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|map| map.values())
        .filter_map(|namespace| {
            let id = namespace.get("id")?.as_i64()? as i32;
            let mut names = aliases
                .iter()
                .filter(|(alias_id, _)| *alias_id == id)
                .map(|(_, alias)| alias.clone())
                .collect::<Vec<_>>();
            if let Some(canonical) = namespace.get("canonical").and_then(Value::as_str) {
                if !canonical.is_empty() {
                    names.push(canonical.to_string());
                }
            }
            names.sort();
            names.dedup();
            Some(SiteNamespaceRecord {
                id,
                case: string(namespace, "case"),
                localized_name: string(namespace, "name"),
                aliases: names,
            })
        })
        .collect::<Vec<_>>();
    namespaces.sort_by_key(|namespace| namespace.id);

    let interwiki = query
        .get("interwikimap")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some(SiteInterwikiRecord {
                prefix: entry.get("prefix")?.as_str()?.to_string(),
                url: entry.get("url")?.as_str()?.to_string(),
                is_local: entry.get("local").is_some(),
            })
        })
        .collect();
    let magic_words = query
        .get("magicwords")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|word| {
            Some(SiteMagicWordRecord {
                canonical_name: word.get("name")?.as_str()?.to_string(),
                aliases: word
                    .get("aliases")?
                    .as_array()?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
                case_sensitive: bool_value(word, "case-sensitive"),
            })
        })
        .collect();
    let site_info = SiteInfoRecord {
        site_name: string(general, "sitename"),
        db_name: string(general, "wikiid"),
        base: string(general, "base"),
        generator: string(general, "generator"),
        case: string(general, "case"),
        language: string(general, "lang"),
        rtl: bool_value(general, "rtl"),
        server: string(general, "server"),
        script_path: string(general, "scriptpath"),
        namespaces,
        interwiki,
        magic_words,
    };
    if site_info.db_name.is_empty() || site_info.namespaces.is_empty() {
        return Err(parse_error("siteinfo response is incomplete"));
    }

    let mut writer = ArchiveWriter::new(
        std::fs::File::create(output)?,
        DEFAULT_FRAME_TARGET,
    )
    .map_err(map_archive)?;
    writer
        .write(&Record::SiteInfo {
            timestamp_micros: chrono::Utc::now().timestamp_micros(),
            site_info,
        })
        .map_err(map_archive)?;
    writer.finish().map_err(map_archive)?;
    Ok(())
}

fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// MediaWiki's API has emitted `rtl` both as a JSON boolean and, in older
/// compatibility responses, as a string.  Presence is not a valid test:
/// `rtl: false` is still present and must keep LTR wikis LTR.
fn bool_value(value: &Value, key: &str) -> bool {
    match value.get(key) {
        Some(Value::Bool(value)) => *value,
        Some(Value::String(value)) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Some(Value::Number(value)) => value.as_i64().is_some_and(|value| value != 0),
        _ => false,
    }
}

fn parse_error(message: impl Into<String>) -> Error {
    Error::Mediawiki(wikimak_mediawiki::Error::Parse(message.into()))
}

fn map_archive(error: crate::archive::ArchiveError) -> Error {
    match error {
        crate::archive::ArchiveError::Mirror(error) => error,
        other => parse_error(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::bool_value;
    use serde_json::json;

    #[test]
    fn boolean_siteinfo_fields_use_the_value_not_presence() {
        let value = json!({
            "false_bool": false,
            "true_bool": true,
            "false_string": "false",
            "true_string": "true",
            "zero": 0,
            "one": 1,
        });
        assert!(!bool_value(&value, "false_bool"));
        assert!(bool_value(&value, "true_bool"));
        assert!(!bool_value(&value, "false_string"));
        assert!(bool_value(&value, "true_string"));
        assert!(!bool_value(&value, "zero"));
        assert!(bool_value(&value, "one"));
    }
}
