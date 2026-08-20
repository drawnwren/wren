use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::tempdir;
use wren_engine::{Editor, Mode, SearchDirection};
use wren_grammar::{KeyCode, KeyEvent, Modifiers, ParseState};
use wren_text::{DefaultText, TextStore};

use crate::oracle::{Oracle, OracleState};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceStep {
    pub keys: String,
    pub state: OracleState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoldenTrace {
    pub oracle_version: String,
    pub scenario: String,
    pub initial: OracleState,
    pub steps: Vec<TraceStep>,
}

struct Scenario {
    name: &'static str,
    lines: &'static [&'static str],
    keys: &'static [&'static str],
}

const SCENARIOS: &[Scenario] = &[
    Scenario { name: "motions", lines: &["one two three", "four five", "six"], keys: &["w", "2j", "0", "$"] },
    Scenario { name: "operators", lines: &["one two three", "four five", "six"], keys: &["dw", "dd", "cwX<Esc>"] },
    Scenario { name: "registers", lines: &["alpha", "beta", "gamma"], keys: &["\"ayy", "j", "\"ap"] },
    Scenario { name: "counts-and-text-objects", lines: &["alpha beta gamma", "delta epsilon", "zeta"], keys: &["2w", "d2w", "u", "ciwX<Esc>"] },
    Scenario { name: "marks", lines: &["alpha", "beta", "gamma"], keys: &["ma", "2j", "`a"] },
    Scenario { name: "macros-and-repeat", lines: &["alpha", "beta", "gamma"], keys: &["qaA!<Esc>q", "j", "@a", "."] },
    Scenario { name: "single-key-edits", lines: &["alpha", "beta", "gamma"], keys: &["x", "u", "rZ", "J", "u", "<C-R>"] },
    Scenario { name: "visual-operators", lines: &["alpha beta", "second", "third"], keys: &["vwd", "u", "Vjy"] },
    Scenario {
        name: "extended-native-grammar",
        lines: &["aa.bb cc-dd (x[y])", "", "para", "", "last"],
        keys: &["E", "W", "B", "f(", "%", "}", "{", "gg0", "dt-", "u", "sX<Esc>", "2~"],
    },
];

pub fn record_goldens(root: &Path) -> Result<PathBuf> {
    let mut oracle = Oracle::spawn()?;
    let destination = root.join(format!("nvim-{}", oracle.version()));
    fs::create_dir_all(&destination).with_context(|| format!("create {}", destination.display()))?;
    for scenario in SCENARIOS {
        oracle.reset(scenario.lines)?;
        let initial = oracle.snapshot()?;
        let mut steps = Vec::new();
        for keys in scenario.keys {
            oracle.input(keys)?;
            steps.push(TraceStep { keys: (*keys).to_owned(), state: oracle.snapshot()? });
        }
        let trace = GoldenTrace { oracle_version: oracle.version().to_owned(), scenario: scenario.name.to_owned(), initial, steps };
        let path = destination.join(format!("{}.json", scenario.name));
        fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&trace)?)).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(destination)
}

pub fn check_determinism() -> Result<()> {
    let first = tempdir()?;
    let second = tempdir()?;
    let first_output = record_goldens(first.path())?;
    let second_output = record_goldens(second.path())?;
    for scenario in SCENARIOS {
        let filename = format!("{}.json", scenario.name);
        let left = fs::read(first_output.join(&filename))?;
        let right = fs::read(second_output.join(&filename))?;
        if left != right {
            bail!("golden regeneration is nondeterministic for {filename}");
        }
    }
    Ok(())
}

pub fn check_wren_against_goldens() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("goldens/nvim-0.12.4");
    for scenario in SCENARIOS {
        let path = root.join(format!("{}.json", scenario.name));
        let golden: GoldenTrace = serde_json::from_slice(&fs::read(&path).with_context(|| format!("read {}", path.display()))?)?;
        let store = DefaultText::from_reader(std::io::Cursor::new(scenario.lines.join("\n")))?;
        let mut editor = Editor::new(store);
        seed_registers(&mut editor, &golden.initial.registers);
        compare_core_state(scenario.name, "initial", &editor, &golden.initial)?;
        for step in &golden.steps {
            feed_wren(&mut editor, &step.keys)?;
            compare_core_state(scenario.name, &step.keys, &editor, &step.state)?;
        }
    }
    Ok(())
}

fn seed_registers(editor: &mut Editor, expected: &Value) {
    let Some(registers) = expected.as_array() else {
        return;
    };
    for expected in registers {
        let Some(name) = expected.get("name").and_then(Value::as_str).and_then(|name| name.chars().next()) else {
            continue;
        };
        let expected_type = expected.get("type").and_then(Value::as_str).unwrap_or("");
        let Some(lines) = expected.get("value").and_then(Value::as_array) else {
            continue;
        };
        if lines.is_empty() {
            continue;
        }
        let mut text = lines.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n");
        if expected_type == "V" {
            text.push('\n');
        }
        editor.restore_register(name, text, expected_type == "V");
    }
}

fn feed_wren(editor: &mut Editor, input: &str) -> Result<()> {
    let mut rest = input;
    while !rest.is_empty() {
        let (key, after) = if let Some(after) = rest.strip_prefix("<Esc>") {
            (KeyEvent::plain(KeyCode::Escape), after)
        } else if let Some(after) = rest.strip_prefix("<C-R>") {
            (KeyEvent { code: KeyCode::Char('r'), modifiers: Modifiers::CONTROL }, after)
        } else {
            let character = rest.chars().next().context("missing key character")?;
            (KeyEvent::character(character), &rest[character.len_utf8()..])
        };
        editor.handle_key(key).with_context(|| format!("wren rejected key trace {input:?}"))?;
        rest = after;
    }
    Ok(())
}

fn compare_core_state(scenario: &str, step: &str, editor: &Editor, expected: &OracleState) -> Result<()> {
    let mode = match editor.mode() {
        Mode::Normal => "n",
        Mode::Insert => "i",
        Mode::Replace => "R",
        Mode::Visual => "v",
        Mode::VisualLine => "V",
    };
    let pending = if editor.pending_parse_state() == Some(ParseState::Operator) { "no" } else { "" };
    let buffer: Vec<_> = editor.contents().split('\n').map(str::to_owned).collect();
    let (line, column) = editor.cursor_line_column();
    let actual_cursor = json!([line + 1, column + 1]);
    let mut differences = Vec::new();
    if mode != expected.mode {
        differences.push(format!("mode expected {:?}, got {mode:?}", expected.mode));
    }
    if pending != expected.pending_operator {
        differences.push(format!("pending operator expected {:?}, got {pending:?}", expected.pending_operator));
    }
    if buffer != expected.buffer {
        differences.push(format!("buffer expected {:?}, got {buffer:?}", expected.buffer));
    }
    if actual_cursor != expected.cursor {
        differences.push(format!("cursor expected {}, got {actual_cursor}", expected.cursor));
    }
    compare_registers(editor, &expected.registers, &mut differences);
    compare_selections(editor, &expected.selections, &mut differences);
    compare_marks(editor, &expected.marks, &mut differences);
    compare_jumplist(editor, &expected.jumplist, &mut differences);
    compare_changelist(editor, &expected.changelist, &mut differences);
    compare_search(editor, &expected.search, &mut differences);
    compare_messages(editor, &expected.messages, &mut differences);
    compare_undo_tree(editor, &expected.undo_tree, &mut differences);
    compare_options(&expected.options, &mut differences);
    if differences.is_empty() { Ok(()) } else { bail!("Neovim differential mismatch in {scenario} after {step:?}: {}", differences.join("; ")) }
}

fn compare_changelist(editor: &Editor, expected: &Value, differences: &mut Vec<String>) {
    let actual_entries = editor.changelist().map(|byte| line_column(editor, byte)).collect::<Vec<_>>();
    compare_history("changelist", expected, actual_entries, editor.change_index(), true, differences);
}

fn compare_options(expected: &Value, differences: &mut Vec<String>) {
    let actual = json!({
        "tabstop": 8,
        "shiftwidth": 8,
        "expandtab": 0,
        "selection": "inclusive",
        "virtualedit": "",
        "whichwrap": "b,s",
    });
    if &actual != expected {
        differences.push(format!("semantics-affecting options expected {expected}, got {actual}"));
    }
}

fn compare_selections(editor: &Editor, expected: &Value, differences: &mut Vec<String>) {
    let expected_start = expected.get("start").and_then(selection_position);
    let expected_end = expected.get("end").and_then(selection_position);
    let expected = expected_start.zip(expected_end);
    let actual = editor.last_visual_selection().map(|selection| ((selection.start_line, selection.start_column), (selection.end_line, selection.end_column)));
    if expected.is_some() && actual != expected {
        differences.push(format!("last visual selection expected {expected:?}, got {actual:?}"));
    }
}

fn selection_position(value: &Value) -> Option<(usize, usize)> {
    let values = value.as_array()?;
    let line = usize::try_from(values.first()?.as_u64()?).ok()?;
    let column = usize::try_from(values.get(1)?.as_u64()?).ok()?;
    if line == 0 {
        return None;
    }
    Some((line - 1, if column >= i32::MAX as usize { usize::MAX } else { column.saturating_sub(1) }))
}

fn compare_marks(editor: &Editor, expected: &Value, differences: &mut Vec<String>) {
    let expected_marks = ["local", "global"]
        .into_iter()
        .flat_map(|kind| expected.get(kind).and_then(Value::as_array).into_iter().flatten())
        .filter_map(|mark| {
            let name = mark.get("mark")?.as_str()?.strip_prefix('\'')?.chars().next()?;
            if !name.is_ascii_alphabetic() {
                return None;
            }
            let position = mark.get("pos")?.as_array()?;
            let line = usize::try_from(position.get(1)?.as_u64()?).ok()?;
            let column = usize::try_from(position.get(2)?.as_u64()?).ok()?;
            Some((name, (line.saturating_sub(1), column.saturating_sub(1))))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let actual = editor
        .marks()
        .filter(|(name, _)| name.is_ascii_alphabetic())
        .map(|(name, byte)| (name, line_column(editor, byte)))
        .collect::<std::collections::BTreeMap<_, _>>();
    if actual != expected_marks {
        differences.push(format!("named marks expected {expected_marks:?}, got {actual:?}"));
    }
}

fn compare_jumplist(editor: &Editor, expected: &Value, differences: &mut Vec<String>) {
    compare_history("jumplist", expected, editor.jumplist().map(|byte| line_column(editor, byte)).collect(), editor.jump_index(), false, differences);
}

fn compare_history(name: &str, expected: &Value, actual_entries: Vec<(usize, usize)>, actual_index: usize, either_edge: bool, differences: &mut Vec<String>) {
    let Some(parts) = expected.as_array() else {
        differences.push(format!("oracle {name} is not an array"));
        return;
    };
    let expected_entries = parts
        .first()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| Some((usize::try_from(entry.get("lnum")?.as_u64()?).ok()?.saturating_sub(1), usize::try_from(entry.get("col")?.as_u64()?).ok()?)))
        .collect::<Vec<_>>();
    let expected_index = parts.get(1).and_then(Value::as_u64).and_then(|value| usize::try_from(value).ok()).unwrap_or(0);
    // Neovim flips the changelist order around its current index after some
    // destructive commands; the jumplist has one stable active edge.
    let same_tail = actual_entries.last() == expected_entries.last() || either_edge && actual_entries.last() == expected_entries.first();
    let same_empty_state = actual_entries.is_empty() == expected_entries.is_empty();
    let same_index_state = (actual_index == 0) == (expected_index == 0);
    if !same_tail || !same_empty_state || !same_index_state {
        differences.push(format!("{name} expected ({expected_entries:?}, {expected_index}), got ({actual_entries:?}, {actual_index})"));
    }
}

fn compare_search(editor: &Editor, expected: &Value, differences: &mut Vec<String>) {
    let pattern = expected.get("pattern").and_then(Value::as_str).unwrap_or("");
    let forward = expected.get("forward").and_then(Value::as_u64) != Some(0);
    let actual = editor.last_search().map_or(("", true), |(pattern, direction)| (pattern, direction == SearchDirection::Forward));
    if actual != (pattern, forward) {
        differences.push(format!("search expected ({pattern:?}, {forward}), got {actual:?}"));
    }
}

fn compare_messages(editor: &Editor, expected: &str, differences: &mut Vec<String>) {
    let actual_nonempty = editor.messages().next().is_some();
    if actual_nonempty == expected.is_empty() {
        differences.push(format!("message-log emptiness expected {}, got {actual_nonempty}", !expected.is_empty()));
    }
}

fn compare_undo_tree(editor: &Editor, expected: &Value, differences: &mut Vec<String>) {
    let expected_total = expected.get("entries").and_then(Value::as_array).map_or(0, |entries| count_undo_entries(entries));
    let expected_at_head = expected_total == 0 || expected.get("seq_cur").and_then(Value::as_u64) == expected.get("seq_last").and_then(Value::as_u64);
    if editor.undo_tree_len() != expected_total || (editor.redo_depth() == 0) != expected_at_head {
        differences.push(format!(
            "undo tree expected total {expected_total}, at_head {expected_at_head}; got total {}, at_head {}",
            editor.undo_tree_len(),
            editor.redo_depth() == 0
        ));
    }
}

fn count_undo_entries(entries: &[Value]) -> usize {
    entries.iter().map(|entry| 1 + entry.get("alt").and_then(Value::as_array).map_or(0, |alternate| count_undo_entries(alternate))).sum()
}

fn line_column(editor: &Editor, byte: usize) -> (usize, usize) {
    let text = editor.contents();
    let byte = byte.min(text.len());
    let line = text[..byte].bytes().filter(|value| *value == b'\n').count();
    let start = text[..byte].rfind('\n').map_or(0, |offset| offset + 1);
    (line, text[start..byte].chars().count())
}

fn compare_registers(editor: &Editor, expected: &Value, differences: &mut Vec<String>) {
    let Some(registers) = expected.as_array() else {
        differences.push("oracle registers are not an array".to_owned());
        return;
    };
    for expected in registers {
        let Some(name) = expected.get("name").and_then(Value::as_str).and_then(|name| name.chars().next()) else {
            continue;
        };
        let expected_type = expected.get("type").and_then(Value::as_str).unwrap_or("");
        let expected_text = expected
            .get("value")
            .and_then(Value::as_array)
            .map(|lines| {
                let mut text = lines.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n");
                if expected_type == "V" && !text.is_empty() {
                    text.push('\n');
                }
                text
            })
            .unwrap_or_default();
        let actual = editor.register(name);
        let actual_text = actual.map_or("", |register| register.text.as_ref());
        let actual_type = actual.map_or("", |register| if register.linewise { "V" } else { "v" });
        if actual_text != expected_text || (!expected_type.is_empty() && actual_type != expected_type) {
            differences.push(format!("register {name:?} expected ({expected_type:?}, {expected_text:?}), got ({actual_type:?}, {actual_text:?})"));
        }
    }
}
