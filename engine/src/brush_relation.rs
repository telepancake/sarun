//! Migration seam between the generic relation and Brush shell syntax.
//!
//! Execution remains on Brush's parser until the gates in
//! `BRUSH-RELATION-MIGRATION.md` are satisfied. The initial code here is only
//! the checked reference harness against which the relation will be measured.

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use serde::Deserialize;

    #[derive(Deserialize)]
    struct ReferenceCase {
        name: String,
        mode: String,
        status: String,
        source: String,
    }

    fn reference_status(case: &ReferenceCase) -> &'static str {
        let mut options = brush_parser::ParserOptions::default();
        match case.mode.as_str() {
            "bash" => {}
            "posix" => {
                options.posix_mode = true;
                options.sh_mode = true;
            }
            other => panic!("{}: unknown parser mode {other:?}", case.name),
        }
        let reader = Cursor::new(case.source.as_bytes());
        let mut parser = brush_parser::Parser::new(reader, &options);
        match parser.parse_program() {
            Ok(_) => "complete",
            Err(brush_parser::ParseError::Tokenizing { inner, .. })
                if inner.is_incomplete() =>
            {
                "incomplete"
            }
            Err(brush_parser::ParseError::ParsingAtEndOfInput) => "incomplete",
            Err(_) => "invalid",
        }
    }

    #[test]
    fn reference_corpus_pins_shell_parse_status() {
        let cases: Vec<ReferenceCase> = serde_json::from_str(include_str!(
            "../testdata/brush_relation_reference.json"
        ))
        .expect("valid Brush relation reference corpus");
        assert!(cases.len() >= 14);
        for case in &cases {
            assert_eq!(
                reference_status(case),
                case.status,
                "reference behavior changed for {}",
                case.name
            );
        }
    }
}
