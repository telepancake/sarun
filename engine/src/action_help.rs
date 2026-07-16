//! Local presentation of action metadata projected from the central relation.
//!
//! Help is client-side representation conversion. It never crosses ui.sock
//! and does not require a running engine.

use crate::generated_wire::ActionHelpRow;

pub fn ui_rows(filter: &str) -> Result<Vec<ActionHelpRow>, String> {
    crate::prolog::global()?.ui_action_help_matching(filter)
}

pub fn rows(filter: &str) -> Result<Vec<ActionHelpRow>, String> {
    crate::prolog::global()?.action_help_matching(filter)
}

pub fn render_rows(rows: &[ActionHelpRow], prefix: &str) -> String {
    let width = rows
        .iter()
        .map(|row| signature(row).len())
        .max()
        .unwrap_or(0);
    let mut output = String::new();
    for row in rows {
        let signature = signature(row);
        output.push_str(&format!(
            "{prefix}{signature:<width$}  {}\n",
            row.description.as_str()
        ));
    }
    output
}

pub fn cli(arguments: &[String]) -> i32 {
    if arguments.len() > 1 {
        eprintln!("usage: sarun verbs [FILTER]");
        return 2;
    }
    let filter = arguments.first().map(String::as_str).unwrap_or("");
    match rows(filter) {
        Ok(rows) if rows.is_empty() => {
            eprintln!("no actions match {filter:?}");
            1
        }
        Ok(rows) => {
            print!("{}", render_rows(&rows, ""));
            0
        }
        Err(error) => {
            eprintln!("sarun: action relation: {error}");
            1
        }
    }
}

fn signature(row: &ActionHelpRow) -> String {
    if row.arguments.as_str().is_empty() {
        row.verb.as_str().to_string()
    } else {
        format!("{} {}", row.verb.as_str(), row.arguments.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_help_needs_no_engine_and_comes_from_the_relation() {
        let rows = ui_rows("mirror").unwrap();
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|row| {
            row.verb.as_str().contains("mirror") || row.description.as_str().contains("mirror")
        }));
        let rendered = render_rows(&rows, "  ");
        assert!(rendered.contains("mirror add"));
        assert!(rendered.contains("add a scheduled"));
    }
}
