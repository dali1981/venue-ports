//! The response catalogue's honesty test (`specs/V7-production-validation.md`).
//!
//! `docs/responses/*.md` has one row for each call × response variant: the body (a path under
//! `fixtures/`), its origin, the test that parses it and the typed result. [`problems`] holds the
//! rules that keep the table from drifting from the files and the tests it describes:
//!
//! - every file under `fixtures/` (a `README.md` apart) is the body of a row;
//! - every row's body exists, under a directory named for the row's origin;
//! - every row that has a body names the test that reads it, and that test exists, in a source file
//!   that names the body;
//! - a row that is `not provokable` has neither.
//!
//! It runs on the files of this repository as `catalogue_matches_fixtures`, and on literals as the
//! tests of its own rules.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const ORIGINS: [&str; 5] = [
    "production",
    "testnet",
    "documented",
    "synthetic",
    "not provokable",
];
/// A cell with nothing in it.
const NONE: &str = "—";

/// One row of a catalogue table.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    /// `docs/responses/binance-spot.md:17`, for the message.
    at: String,
    call: String,
    body: String,
    origin: String,
    test: String,
}

/// What [`problems`] checks the rows against.
#[derive(Debug, Default)]
struct World {
    /// Every file under `fixtures/` that is a body, as `fixtures/…`.
    fixtures: BTreeSet<String>,
    /// Every Rust source file by its path from the crate root (`src/cex/mod.rs`), with its text.
    sources: BTreeMap<String, String>,
}

/// The rows of every table in `text` (a line that starts with `|`, the header and the rule apart).
fn rows(file: &str, text: &str) -> Vec<Row> {
    let cell = |cell: &str| cell.trim().trim_matches('`').trim().to_string();
    text.lines()
        .enumerate()
        .filter(|(_, line)| line.trim_start().starts_with('|'))
        .filter_map(|(index, line)| {
            let cells: Vec<String> = line
                .trim()
                .trim_start_matches('|')
                .trim_end_matches('|')
                .split('|')
                .map(cell)
                .collect();
            let is_rule = cells
                .iter()
                .all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':'));
            if is_rule || cells.first().is_some_and(|c| c == "Call") {
                return None;
            }
            let at = format!("{file}:{}", index + 1);
            Some(match <[String; 6]>::try_from(cells) {
                Ok([call, _variant, body, origin, test, _typed]) => Row {
                    at,
                    call,
                    body,
                    origin,
                    test,
                },
                // Not six cells: kept, so that `problems` says so.
                Err(cells) => Row {
                    at,
                    call: format!("{} cells, not 6", cells.len()),
                    body: String::new(),
                    origin: String::new(),
                    test: String::new(),
                },
            })
        })
        .collect()
}

/// The source file a test path (`cex::binance::live::tests::name`) lives in: the longest prefix of
/// its module path that is a file, as `src/…/x.rs` or `src/…/x/mod.rs`.
fn source_of<'a>(world: &'a World, test: &str) -> Option<(&'a str, &'a str)> {
    let mut modules: Vec<&str> = test.split("::").collect();
    modules.pop()?;
    while !modules.is_empty() {
        let joined = modules.join("/");
        for candidate in [format!("src/{joined}.rs"), format!("src/{joined}/mod.rs")] {
            if let Some((path, text)) = world.sources.get_key_value(&candidate) {
                return Some((path.as_str(), text.as_str()));
            }
        }
        modules.pop();
    }
    None
}

/// Every way `rows` disagree with `world`, one line each.
fn problems(rows: &[Row], world: &World) -> Vec<String> {
    let mut out = Vec::new();
    let mut read = BTreeSet::new();
    for row in rows {
        let at = &row.at;
        if row.origin.is_empty() {
            out.push(format!("{at}: {}", row.call));
            continue;
        }
        if !ORIGINS.contains(&row.origin.as_str()) {
            out.push(format!(
                "{at}: origin `{}` is none of {ORIGINS:?}",
                row.origin
            ));
        }
        let has_body = row.body != NONE;
        let has_test = row.test != NONE;
        if row.origin == "not provokable" {
            if has_body || has_test {
                out.push(format!(
                    "{at}: a case that is not provokable has no body and no test"
                ));
            }
            continue;
        }
        if has_body && !has_test {
            out.push(format!("{at}: the body {} is read by no test", row.body));
        }
        if has_body {
            read.insert(row.body.clone());
            if !world.fixtures.contains(&row.body) {
                out.push(format!("{at}: the body {} does not exist", row.body));
            } else if !row.body.contains(&format!("/{}/", row.origin)) {
                out.push(format!(
                    "{at}: the body {} is not under a directory named `{}`, its origin",
                    row.body, row.origin
                ));
            }
        }
        if has_test {
            match source_of(world, &row.test) {
                None => out.push(format!(
                    "{at}: the test {} is in no source file of this crate",
                    row.test
                )),
                Some((path, text)) => {
                    let name = row.test.rsplit("::").next().unwrap_or_default();
                    if !text.contains(&format!("fn {name}(")) {
                        out.push(format!("{at}: {path} has no fn {name}"));
                    }
                    if let Some(suffix) = row.body.strip_prefix("fixtures/") {
                        if has_body && !text.contains(suffix) {
                            out.push(format!(
                                "{at}: {path} does not name the body {}, so {} cannot read it",
                                row.body, row.test
                            ));
                        }
                    }
                }
            }
        }
        if !has_body && !has_test && row.call.is_empty() {
            out.push(format!("{at}: a row with no call"));
        }
    }
    for fixture in &world.fixtures {
        if !read.contains(fixture) {
            out.push(format!("{fixture} has no row in docs/responses"));
        }
    }
    out
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// This repository's fixtures, sources and catalogue.
fn this_repository() -> (World, Vec<Row>) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let relative = |path: &Path| {
        path.strip_prefix(root)
            .expect("under the crate root")
            .to_string_lossy()
            .replace('\\', "/")
    };
    let mut world = World::default();

    let mut fixtures = Vec::new();
    files_under(&root.join("fixtures"), &mut fixtures);
    for path in fixtures {
        if path.file_name().is_some_and(|name| name == "README.md") {
            continue;
        }
        world.fixtures.insert(relative(&path));
    }

    let mut sources = Vec::new();
    files_under(&root.join("src"), &mut sources);
    for path in sources {
        if path.extension().is_some_and(|ext| ext == "rs") {
            let text = std::fs::read_to_string(&path).expect("a readable source file");
            world.sources.insert(relative(&path), text);
        }
    }

    let mut docs = Vec::new();
    files_under(&root.join("docs/responses"), &mut docs);
    let mut all = Vec::new();
    for path in docs {
        if path.extension().is_some_and(|ext| ext == "md") {
            let text = std::fs::read_to_string(&path).expect("a readable catalogue");
            all.extend(rows(&relative(&path), &text));
        }
    }
    (world, all)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_matches_fixtures() {
        let (world, rows) = this_repository();
        assert!(
            !rows.is_empty(),
            "docs/responses/*.md has no rows: the catalogue is missing"
        );
        let problems = problems(&rows, &world);
        assert!(
            problems.is_empty(),
            "the catalogue and the fixtures disagree:\n{}",
            problems.join("\n")
        );
    }

    fn world() -> World {
        World {
            fixtures: BTreeSet::from(["fixtures/v/documented/a.json".to_string()]),
            sources: BTreeMap::from([(
                "src/x/mod.rs".to_string(),
                "const A: &str = include_str!(\"../../fixtures/v/documented/a.json\");\n\
                 fn reads_a() {}"
                    .to_string(),
            )]),
        }
    }

    fn table(body: &str) -> String {
        format!(
            "| Call | Variant | Body | Origin | Test | Typed result |\n| --- | --- | --- | --- | --- | --- |\n{body}\n"
        )
    }

    fn check(body: &str) -> Vec<String> {
        problems(&rows("c.md", &table(body)), &world())
    }

    const GOOD: &str =
        "| `GET /a` | v | `fixtures/v/documented/a.json` | documented | `x::reads_a` | `Ok` |";

    #[test]
    fn a_row_that_agrees_with_the_files_has_no_problem() {
        assert_eq!(check(GOOD), Vec::<String>::new());
    }

    #[test]
    fn the_header_and_the_rule_are_not_rows() {
        let all = rows("c.md", &table(GOOD));
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].at, "c.md:3");
        assert_eq!(all[0].test, "x::reads_a");
    }

    #[test]
    fn a_fixture_with_no_row_is_reported() {
        let problems = check("");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("fixtures/v/documented/a.json has no row"));
    }

    #[test]
    fn a_row_whose_body_is_missing_is_reported() {
        let problems = check(
            "| `GET /a` | v | `fixtures/v/documented/b.json` | documented | `x::reads_a` | `Ok` |",
        );
        assert!(problems.iter().any(|p| p.contains("b.json does not exist")));
    }

    #[test]
    fn a_row_whose_test_does_not_exist_is_reported() {
        for test in ["x::no_such_test", "y::reads_a"] {
            let problems = check(&format!(
                "| `GET /a` | v | `fixtures/v/documented/a.json` | documented | `{test}` | `Ok` |"
            ));
            assert!(
                problems
                    .iter()
                    .any(|p| p.contains(test.rsplit("::").next().unwrap())
                        || p.contains("no source file")),
                "{test}: {problems:?}"
            );
        }
    }

    #[test]
    fn a_test_that_does_not_name_the_body_cannot_read_it() {
        let mut world = world();
        world
            .sources
            .insert("src/x/mod.rs".to_string(), "fn reads_a() {}".to_string());
        let problems = problems(&rows("c.md", &table(GOOD)), &world);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("does not name the body")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_body_with_no_test_is_reported() {
        let problems =
            check("| `GET /a` | v | `fixtures/v/documented/a.json` | documented | — | `Ok` |");
        assert!(
            problems.iter().any(|p| p.contains("read by no test")),
            "{problems:?}"
        );
    }

    #[test]
    fn an_origin_must_be_one_of_the_five_and_name_the_directory() {
        let problems = check(
            "| `GET /a` | v | `fixtures/v/documented/a.json` | production | `x::reads_a` | `Ok` |",
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("is not under a directory named `production`")),
            "{problems:?}"
        );
        let problems = check(
            "| `GET /a` | v | `fixtures/v/documented/a.json` | invented | `x::reads_a` | `Ok` |",
        );
        assert!(
            problems.iter().any(|p| p.contains("invented")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_case_that_is_not_provokable_has_no_body_and_no_test() {
        let ok = problems(
            &rows(
                "c.md",
                &table(&format!(
                    "{GOOD}\n| any | a ban | — | not provokable | — | — |"
                )),
            ),
            &world(),
        );
        assert_eq!(ok, Vec::<String>::new());
        let bad = problems(
            &rows(
                "c.md",
                &table(&format!(
                    "{GOOD}\n| any | a ban | `fixtures/v/documented/a.json` | not provokable | — | — |"
                )),
            ),
            &world(),
        );
        assert!(bad.iter().any(|p| p.contains("not provokable")), "{bad:?}");
    }

    #[test]
    fn a_row_with_the_wrong_number_of_cells_is_reported() {
        let problems = check("| `GET /a` | v | — | documented |");
        assert!(
            problems.iter().any(|p| p.contains("4 cells, not 6")),
            "{problems:?}"
        );
    }
}
