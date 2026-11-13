const FIXTURE_TEST_MAX_PARSE_DEPTH: usize = 1024;

/// First difference between two parse-tree YAML values, or `None` if equal.
/// Mapping key order matters (it's child order); `_hash` is ignored. Scalars
/// compare by string form; a string vs a number compares numerically, since
/// pyyaml (YAML 1.1) leaves `1e5` unquoted and serde_yaml_ng (YAML 1.2) reads
/// it back as a float.
fn first_yaml_diff(
    gen: &serde_yaml_ng::Value,
    exp: &serde_yaml_ng::Value,
    path: &str,
) -> Option<String> {
    use serde_yaml_ng::Value;
    fn scalar(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            Value::Null => Some("null".to_string()),
            _ => None,
        }
    }
    fn num(v: &Value) -> Option<f64> {
        match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        }
    }
    let short = |v: &Value| {
        let s = serde_yaml_ng::to_string(v).unwrap_or_default();
        let s = s.trim().replace('\n', "\n            ");
        if s.len() > 200 {
            format!("{}...", &s[..s.floor_char_boundary(200)])
        } else {
            s.to_string()
        }
    };
    match (gen, exp) {
        (Value::Mapping(g), Value::Mapping(e)) => {
            let is_hash = |k: &&Value| k.as_str() == Some("_hash");
            let gk: Vec<_> = g.keys().filter(|k| !is_hash(k)).collect();
            let ek: Vec<_> = e.keys().filter(|k| !is_hash(k)).collect();
            if gk != ek {
                return Some(format!(
                    "{}: keys differ\n    rust:   {:?}\n    python: {:?}",
                    if path.is_empty() { "<root>" } else { path },
                    gk.iter()
                        .map(|k| scalar(k).unwrap_or_default())
                        .collect::<Vec<_>>(),
                    ek.iter()
                        .map(|k| scalar(k).unwrap_or_default())
                        .collect::<Vec<_>>(),
                ));
            }
            gk.iter().find_map(|k| {
                let p = format!("{}/{}", path, scalar(k).unwrap_or_default());
                first_yaml_diff(&g[*k], &e[*k], &p)
            })
        }
        (Value::Sequence(g), Value::Sequence(e)) => {
            let common = g
                .iter()
                .zip(e)
                .enumerate()
                .find_map(|(i, (gv, ev))| first_yaml_diff(gv, ev, &format!("{}[{}]", path, i)));
            common.or_else(|| {
                (g.len() != e.len()).then(|| {
                    format!(
                        "{}: length differs (rust {}, python {})",
                        path,
                        g.len(),
                        e.len()
                    )
                })
            })
        }
        _ => match (scalar(gen), scalar(exp)) {
            (Some(a), Some(b)) if a == b => None,
            (_, _)
                if matches!(
                    (gen, exp),
                    (Value::String(_), Value::Number(_)) | (Value::Number(_), Value::String(_))
                ) && num(gen).is_some()
                    && num(gen) == num(exp) =>
            {
                None
            }
            _ => Some(format!(
                "{}\n    rust:   {}\n    python: {}",
                path,
                short(gen),
                short(exp)
            )),
        },
    }
}

/// Compare Rust YAML output to Python YAML for all fixtures in a given dialect.
fn check_yaml_output_matches_python_for_dialect(dialect: &str) {
    env_logger::try_init().ok();

    let fixtures_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("test/fixtures");

    let tests = FixtureTest::discover(dialect, &fixtures_root);
    let mut total = 0;
    let mut failed = 0;
    let mut failed_tests = Vec::new();

    for test in &tests {
        // Read SQL file
        let sql_content = std::fs::read_to_string(&test.sql_path).expect("Failed to read SQL file");
        // Read expected YAML
        let expected_yaml =
            std::fs::read_to_string(&test.yml_path).expect("Failed to read expected YAML file");

        // Parse with Rust parser
        let dialect_obj =
            sqlfluffrs_dialects::Dialect::from_str(&test.dialect).expect("Invalid dialect");
        let input = sqlfluffrs_lexer::LexInput::String(sql_content.clone());
        let lexer = sqlfluffrs_lexer::Lexer::for_dialect(dialect_obj);
        let (tokens, lex_errors) = lexer.lex(input, false);
        assert!(lex_errors.is_empty(), "Lexer errors: {:?}", lex_errors);
        let mut parser = sqlfluffrs_parser::parser::Parser::new_with_max_parse_depth(
            &tokens,
            dialect_obj,
            hashbrown::HashMap::new(),
            FIXTURE_TEST_MAX_PARSE_DEPTH,
        );
        let ast = parser
            .call_rule_as_root()
            .expect("Parse error")
            .apply_as_root(&tokens, &[], &[]);

        total += 1;
        // Compare the record itself: re-reading node_to_yaml's text would lose
        // quotes to its pyyaml-style post-processing.
        let gen_val = ast.as_record(true, true, false).expect("as_record");
        let exp_val: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&expected_yaml).expect("Expected YAML not valid");

        if let Some(diff) = first_yaml_diff(&gen_val, &exp_val, "") {
            failed += 1;
            failed_tests.push(test.name.clone());
            println!(
                "\n=== YAML MISMATCH: {}::{} ===\n  {}",
                dialect, test.name, diff
            );
        }
    }

    println!(
        "\nYAML output comparison for dialect '{}': {} total, {} failed",
        dialect, total, failed
    );
    if !failed_tests.is_empty() {
        println!("Failed tests for dialect '{}':", dialect);
        for name in &failed_tests {
            println!("  {}::{}", dialect, name);
        }
        panic!(
            "Some YAML outputs did not match Python reference for dialect '{}'",
            dialect
        );
    }
}

fn process_yaml_11(yaml_str: String) -> String {
    // Post-process: quote values in the given list in single quotes to match pyyaml safe_dump
    let unquoted_keywords = ["NO", "YES", "ON", "OFF", "NULL", "TRUE", "FALSE", "="];
    let quoted = yaml_str
        .lines()
        .map(|line| {
            if let Some((k, v)) = line.split_once(": ") {
                // Convert triple single quoted strings to single quoted
                let v = if v.starts_with("'''") && v.ends_with("'''") && v.len() > 6 {
                    let inner = &v[3..v.len() - 3];
                    format!("'{}'", inner.replace("'", "''"))
                } else {
                    v.to_string()
                };
                // Only quote if value is in the list and not already quoted
                if unquoted_keywords.contains(&v.to_uppercase().as_str())
                    && !v.starts_with('"')
                    && !v.starts_with('\'')
                {
                    format!("{}: '{}'", k, v)
                } else {
                    format!("{}: {}", k, v)
                }
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{}\n", quoted)
}

#[test]
fn test_yaml_output_matches_python_ansi() {
    check_yaml_output_matches_python_for_dialect("ansi");
}

#[test]
fn test_yaml_output_matches_python_athena() {
    check_yaml_output_matches_python_for_dialect("athena");
}

#[test]
fn test_yaml_output_matches_python_bigquery() {
    check_yaml_output_matches_python_for_dialect("bigquery");
}

#[test]
fn test_yaml_output_matches_python_clickhouse() {
    check_yaml_output_matches_python_for_dialect("clickhouse");
}

#[test]
fn test_yaml_output_matches_python_databricks() {
    check_yaml_output_matches_python_for_dialect("databricks");
}

#[test]
fn test_yaml_output_matches_python_db2() {
    check_yaml_output_matches_python_for_dialect("db2");
}

#[test]
fn test_yaml_output_matches_python_doris() {
    check_yaml_output_matches_python_for_dialect("doris");
}

#[test]
fn test_yaml_output_matches_python_duckdb() {
    check_yaml_output_matches_python_for_dialect("duckdb");
}

#[test]
fn test_yaml_output_matches_python_exasol() {
    check_yaml_output_matches_python_for_dialect("exasol");
}

#[test]
fn test_yaml_output_matches_python_flink() {
    check_yaml_output_matches_python_for_dialect("flink");
}

#[test]
fn test_yaml_output_matches_python_greenplum() {
    check_yaml_output_matches_python_for_dialect("greenplum");
}

#[test]
fn test_yaml_output_matches_python_hive() {
    check_yaml_output_matches_python_for_dialect("hive");
}

#[test]
fn test_yaml_output_matches_python_impala() {
    check_yaml_output_matches_python_for_dialect("impala");
}

#[test]
fn test_yaml_output_matches_python_mariadb() {
    check_yaml_output_matches_python_for_dialect("mariadb");
}

#[test]
fn test_yaml_output_matches_python_materialize() {
    check_yaml_output_matches_python_for_dialect("materialize");
}

#[test]
fn test_yaml_output_matches_python_mysql() {
    check_yaml_output_matches_python_for_dialect("mysql");
}

#[test]
fn test_yaml_output_matches_python_oracle() {
    check_yaml_output_matches_python_for_dialect("oracle");
}

#[test]
fn test_yaml_output_matches_python_postgres() {
    check_yaml_output_matches_python_for_dialect("postgres");
}

#[test]
fn test_yaml_output_matches_python_redshift() {
    check_yaml_output_matches_python_for_dialect("redshift");
}

#[test]
fn test_yaml_output_matches_python_snowflake() {
    check_yaml_output_matches_python_for_dialect("snowflake");
}

#[test]
fn test_yaml_output_matches_python_soql() {
    check_yaml_output_matches_python_for_dialect("soql");
}

#[test]
fn test_yaml_output_matches_python_sparksql() {
    check_yaml_output_matches_python_for_dialect("sparksql");
}

#[test]
fn test_yaml_output_matches_python_sqlite() {
    check_yaml_output_matches_python_for_dialect("sqlite");
}

#[test]
fn test_yaml_output_matches_python_starrocks() {
    check_yaml_output_matches_python_for_dialect("starrocks");
}

#[test]
fn test_yaml_output_matches_python_teradata() {
    check_yaml_output_matches_python_for_dialect("teradata");
}

#[test]
fn test_yaml_output_matches_python_trino() {
    check_yaml_output_matches_python_for_dialect("trino");
}

#[test]
fn test_yaml_output_matches_python_tsql() {
    check_yaml_output_matches_python_for_dialect("tsql");
}

#[test]
fn test_yaml_output_matches_python_vertica() {
    check_yaml_output_matches_python_for_dialect("vertica");
}

// (Imports above are already present in this file; do not re-import.)
use serde_yaml_ng::Value;
use sqlfluffrs_dialects::Dialect;
use sqlfluffrs_lexer::{LexInput, Lexer};
use std::fs;
use std::path::PathBuf;

/// Helper: Generate YAML from AST using the same logic as examples/parse_fixture.rs
fn node_to_yaml(
    node: &sqlfluffrs_parser::parser::Node,
    _tokens: &[sqlfluffrs_types::token::Token],
) -> Result<String, Box<dyn std::error::Error>> {
    use serde_yaml_ng::{Mapping, Value};
    let mut root_map = Mapping::new();
    let as_record = node.as_record(true, true, false);
    root_map.insert(
        Value::String("_hash".to_string()),
        Value::String("PLACEHOLDER_HASH".to_string()),
    );
    if let Some(Value::Mapping(m)) = as_record {
        for (k, v) in m {
            root_map.insert(k, v);
        }
    } else {
        root_map.insert(
            Value::String("node".to_string()),
            as_record.expect("Node as_record should not be None"),
        );
    }
    let header = "# YML test files are auto-generated from SQL files and should not be edited by\n\
                  # hand. To help enforce this, the \"hash\" field in the file must match a hash\n\
                  # computed by SQLFluff when running the tests. Please run\n\
                  # `python test/generate_parse_fixture_yml.py`  to generate them after adding or\n\
                  # altering SQL files.\n";
    let mut yaml_val = Value::Mapping(root_map);
    insert_yaml_hash(&mut yaml_val);
    let yaml_str = serde_yaml_ng::to_string(&yaml_val)?;
    let quoted = process_yaml_11(yaml_str);
    Ok(format!("{}{}", header, quoted))
}

/// Helper: Insert hash into YAML mapping (copied from examples/parse_fixture.rs)
fn insert_yaml_hash(yaml: &mut Value) {
    let hash = compute_yaml_hash(yaml);
    if let Value::Mapping(map) = yaml {
        map.insert(Value::String("_hash".to_string()), Value::String(hash));
    }
}

/// Helper: Compute hash for YAML value (copied from examples/parse_fixture.rs)
fn compute_yaml_hash(yaml: &Value) -> String {
    use blake2::{Blake2s256, Digest};
    let clean = match yaml {
        Value::Mapping(map) => {
            let mut m = map.clone();
            m.remove(Value::String("_hash".to_string()));
            Value::Mapping(m)
        }
        _ => yaml.clone(),
    };
    let yaml_str = process_yaml_11(serde_yaml_ng::to_string(&clean).unwrap());
    let mut hasher = Blake2s256::new();
    hasher.update(yaml_str.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[test]
fn test_yaml_output_matches_python() {
    // Test a representative ANSI fixture (select_simple_a)
    let fixtures_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("test/fixtures");
    let sql_path = fixtures_root.join("dialects/ansi/select_simple_a.sql");
    let yml_path = fixtures_root.join("dialects/ansi/select_simple_a.yml");

    // Read SQL
    let sql_content = fs::read_to_string(&sql_path).expect("Failed to read SQL file");
    // Lex
    let input = LexInput::String(sql_content);
    let dialect = Dialect::Ansi;
    let lexer = Lexer::new(None, dialect.get_lexers().to_vec());
    let (tokens, lex_errors) = lexer.lex(input, false);
    assert!(lex_errors.is_empty(), "Lexer errors: {:?}", lex_errors);
    // Parse
    let mut parser =
        sqlfluffrs_parser::parser::Parser::new(&tokens, Dialect::Ansi, hashbrown::HashMap::new());
    let ast = parser
        .call_rule_as_root()
        .expect("Parse error")
        .apply_as_root(&tokens, &[], &[]);

    // Generate YAML
    let generated_yaml = node_to_yaml(&ast, &tokens).expect("YAML generation failed");
    // Read expected YAML
    let expected_yaml = fs::read_to_string(&yml_path).expect("Failed to read expected YAML");

    // Compare
    if generated_yaml.trim() != expected_yaml.trim() {
        let gen_lines: Vec<&str> = generated_yaml.lines().collect();
        let exp_lines: Vec<&str> = expected_yaml.lines().collect();
        println!("\n=== GENERATED YAML ===\n{}", generated_yaml);
        println!("\n=== EXPECTED YAML ===\n{}", expected_yaml);
        println!("\nDifferences:");
        for (i, (gen_line, exp_line)) in gen_lines.iter().zip(exp_lines.iter()).enumerate() {
            if gen_line != exp_line {
                println!("  Line {}:", i + 1);
                println!("    Generated: {}", gen_line);
                println!("    Expected:  {}", exp_line);
            }
        }
        if gen_lines.len() != exp_lines.len() {
            println!(
                "  Line count differs: Generated: {} lines, Expected: {} lines",
                gen_lines.len(),
                exp_lines.len()
            );
        }
        panic!("YAML output does not match expected Python YAML");
    }
}

/// Integration tests for parsing SQL fixtures
///
/// This test suite parses SQL files from test/fixtures/dialects/ and compares
/// the output against expected YAML files.
use sqlfluffrs_parser::parser::{Node, Parser};
use std::path::Path;
use std::str::FromStr;

struct FixtureTest {
    dialect: String,
    name: String,
    sql_path: PathBuf,
    yml_path: PathBuf,
}

impl FixtureTest {
    fn discover(dialect: &str, fixtures_root: &Path) -> Vec<Self> {
        let dialect_dir = fixtures_root.join("dialects").join(dialect);
        if !dialect_dir.exists() {
            return Vec::new();
        }

        let mut tests = Vec::new();
        if let Ok(entries) = fs::read_dir(&dialect_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("sql") {
                    let name = path.file_stem().unwrap().to_string_lossy().to_string();
                    let yml_path = path.with_extension("yml");

                    // Only include tests that have corresponding YAML files
                    if yml_path.exists() {
                        tests.push(FixtureTest {
                            dialect: dialect.to_string(),
                            name,
                            sql_path: path,
                            yml_path,
                        });
                    }
                }
            }
        }
        tests.sort_by(|a, b| a.name.cmp(&b.name));
        tests
    }

    fn run(&self) -> Result<Node, String> {
        // Read SQL file
        let sql_content = fs::read_to_string(&self.sql_path)
            .map_err(|e| format!("Failed to read SQL file: {}", e))?;

        // Parse with Rust parser
        let dialect = Dialect::from_str(&self.dialect).expect("Invalid dialect");

        let input = LexInput::String(sql_content.clone());
        // Use the correct lexers for the dialect
        let lexer = Lexer::for_dialect(dialect);
        let (tokens, lex_errors) = lexer.lex(input, false);

        if !lex_errors.is_empty() {
            return Err(format!("Lexer errors: {:?}", lex_errors));
        }

        // Use a generous depth limit so stress-test fixtures like
        // expression_recursion don't hit the DoS guard (default 600).
        let mut parser = Parser::new_with_max_parse_depth(
            &tokens,
            dialect,
            hashbrown::HashMap::new(),
            FIXTURE_TEST_MAX_PARSE_DEPTH,
        );

        // Try to parse as a file (top-level rule)
        let ast = parser
            .call_rule_as_root()
            .map_err(|e| format!("Parse error: {:?}", e))?
            .apply_as_root(&tokens, &[], &[]);
        Ok(ast)
    }
}

#[test]
fn test_select_simple_a() {
    env_logger::try_init().ok();

    let fixtures_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("test/fixtures");

    let test = sqlfluffrs::test_harness::FixtureTest {
        dialect: "ansi".to_string(),
        name: "select_simple_a".to_string(),
        sql_path: fixtures_root.join("dialects/ansi/select_simple_a.sql"),
        yml_path: fixtures_root.join("dialects/ansi/select_simple_a.yml"),
    };

    let result = test.run();

    if let Some(yaml) = &result.generated_yaml {
        println!("\n=== Generated YAML ===");
        println!("{}", yaml);
    }

    if let Some(error) = &result.error {
        println!("\n=== Error ===");
        println!("{}", error);
    }

    assert!(result.success, "Test failed: {:?}", result.error);
}

#[test]
fn test_select_a() {
    env_logger::try_init().ok();

    let fixtures_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("test/fixtures");

    let tests = FixtureTest::discover("ansi", &fixtures_root);
    let test = tests
        .iter()
        .find(|t| t.name == "select_a")
        .expect("select_a.sql not found");

    let ast = test.run().expect("Parse failed");
    println!("Successfully parsed select_a");
    println!("AST: {:#?}", ast);
}
