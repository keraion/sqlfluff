//! The engine pipeline (`sqlfluffrs_engine::pipeline`) must build the same
//! tree as the lexer and parser used directly, for every dialect fixture.
//!
//! `fixture_tests.rs` checks the direct path against the fixture YAML. This
//! test ties the engine to that path, so a difference in how the pipeline
//! sets up the lexer or parser shows up here.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use sqlfluffrs_dialects::Dialect;
use sqlfluffrs_engine::pipeline::{self, ParseLimits};
use sqlfluffrs_lexer::{LexInput, Lexer};
use sqlfluffrs_parser::parser::Parser;
use sqlfluffrs_types::TemplatedFile;

// Same depth as fixture_tests.rs: some fixtures nest deeper than the default.
const MAX_PARSE_DEPTH: usize = 1024;

type Record = Result<Option<serde_yaml_ng::Value>, String>;

fn direct_record(sql: &str, dialect: Dialect) -> Record {
    let (tokens, _) = Lexer::for_dialect(dialect).lex(LexInput::String(sql.to_string()), false);
    let mut parser = Parser::new_with_max_parse_depth(
        &tokens,
        dialect,
        hashbrown::HashMap::new(),
        MAX_PARSE_DEPTH,
    );
    let node = parser
        .call_rule_as_root()
        .map_err(|e| format!("{e:?}"))?
        .apply_as_root(&tokens, &[], &[]);
    Ok(node.as_record(true, true, false))
}

fn pipeline_record(sql: &str, dialect: Dialect) -> Record {
    let variant = Arc::new(TemplatedFile::from(sql.to_string()));
    let (tokens, _) = pipeline::lex_variant(&variant, dialect, false);
    let limits = ParseLimits {
        max_parse_depth: MAX_PARSE_DEPTH,
        ..ParseLimits::default()
    };
    let node = pipeline::parse_tokens(&tokens, dialect, BTreeMap::new(), limits)
        .map_err(|e| e.to_string())?;
    Ok(node.as_record(true, true, false))
}

/// Names of the fixtures in one dialect where the two paths differ.
fn mismatches(dialect_dir: &Path, dialect: Dialect) -> (usize, Vec<String>) {
    let mut paths: Vec<PathBuf> = fs::read_dir(dialect_dir)
        .expect("fixture dir")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
        .collect();
    paths.sort();
    let failed = paths
        .iter()
        .filter(|path| {
            let sql = fs::read_to_string(path).expect("read fixture");
            match (direct_record(&sql, dialect), pipeline_record(&sql, dialect)) {
                (Ok(direct), Ok(engine)) => direct != engine,
                // A fixture neither path can parse is not a difference.
                (Err(_), Err(_)) => false,
                _ => true,
            }
        })
        .map(|path| path.display().to_string())
        .collect();
    (paths.len(), failed)
}

#[test]
fn pipeline_matches_direct_parse_for_all_dialect_fixtures() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("test/fixtures/dialects");
    let dialects: Vec<(PathBuf, Dialect)> = fs::read_dir(&root)
        .expect("fixtures root")
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            Dialect::from_str(&name).ok().map(|d| (entry.path(), d))
        })
        .collect();
    assert!(dialects.len() > 20, "found {} dialects", dialects.len());

    // One thread per dialect: this is a debug build over ~2,000 files.
    let results: Vec<(usize, Vec<String>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = dialects
            .iter()
            .map(|(dir, dialect)| scope.spawn(|| mismatches(dir, *dialect)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let total: usize = results.iter().map(|(n, _)| n).sum();
    let failed: Vec<&String> = results.iter().flat_map(|(_, f)| f).collect();
    assert!(total > 1000, "only {total} fixtures found");
    assert!(
        failed.is_empty(),
        "{} of {total} fixtures differ between the engine pipeline and the \
         direct parse:\n{}",
        failed.len(),
        failed
            .iter()
            .map(|f| format!("  {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
