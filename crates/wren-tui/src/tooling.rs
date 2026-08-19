use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FormatterInvocation {
    pub(super) program: String,
    pub(super) arguments: Vec<String>,
}

const FILE_ARGUMENT: &str = "$file";

#[derive(Clone, Copy)]
struct ToolProfile {
    languages: &'static [&'static str],
    program: &'static str,
    arguments: &'static str,
    parent_directory: bool,
}

macro_rules! tool {
    ($languages:expr, $program:literal, $arguments:literal, $parent:literal) => {
        ToolProfile { languages: $languages, program: $program, arguments: $arguments, parent_directory: $parent }
    };
}

const FORMATTER_PROFILES: &[ToolProfile] = &[
    tool!(&["rust"], "rustfmt", "--emit=stdout", false),
    tool!(&["python"], "ruff", "format --stdin-filename $file -", false),
    tool!(&["go"], "gofmt", "", false),
    tool!(&["hcl"], "tofu", "fmt -", false),
    tool!(&["nix"], "nixfmt", "", false),
    tool!(&["haskell"], "fourmolu", "--stdin-input-file $file", false),
];

const DIAGNOSTIC_PROFILES: &[ToolProfile] = &[
    tool!(&["rust"], "cargo", "clippy --quiet --message-format=short", false),
    tool!(&["python"], "ruff", "check --output-format=concise $file", false),
    tool!(&["javascript", "typescript", "tsx"], "pnpm", "exec tsc --noEmit --pretty false", false),
    tool!(&["go"], "go", "vet ./...", false),
    tool!(&["hcl"], "tofu", "validate -no-color", true),
    tool!(&["nix"], "nix-instantiate", "--parse $file", false),
    tool!(&["haskell"], "ghc", "-fno-code $file", false),
    tool!(&["lua"], "luac", "-p $file", false),
    tool!(&["bash"], "bash", "-n $file", false),
    tool!(&["c", "cpp"], "clang++", "-fsyntax-only $file", false),
];

impl ToolProfile {
    fn for_language(profiles: &'static [Self], language: &str) -> Option<&'static Self> {
        profiles.iter().find(|profile| profile.languages.contains(&language))
    }

    fn arguments(&self, path: &Path) -> Vec<String> {
        let file = path.to_string_lossy();
        self.arguments.split_ascii_whitespace().map(|argument| if argument == FILE_ARGUMENT { file.as_ref() } else { argument }).map(str::to_owned).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct IndentStyle {
    pub(super) expand_tabs: bool,
    pub(super) width: usize,
}

pub(super) fn detect_indent_style(text: &str) -> IndentStyle {
    let mut tab_lines = 0;
    let mut widths = Vec::new();
    for line in text.lines().take(2_000) {
        if line.starts_with('\t') {
            tab_lines += 1;
            continue;
        }
        let width = line.bytes().take_while(|byte| *byte == b' ').count();
        if width > 0 && !line.trim().is_empty() {
            widths.push(width);
        }
    }
    if tab_lines > widths.len() {
        return IndentStyle { expand_tabs: false, width: 2 };
    }
    let width = widths.into_iter().filter(|width| *width <= 8).reduce(greatest_common_divisor).filter(|width| *width > 1).unwrap_or(2).clamp(2, 8);
    IndentStyle { expand_tabs: true, width }
}

pub(super) fn greatest_common_divisor(mut left: usize, mut right: usize) -> usize {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

pub(super) fn wrap_editor_text(source: &str, width: usize) -> String {
    let first = source.lines().next().unwrap_or_default();
    let indentation = &first[..first.len().saturating_sub(first.trim_start().len())];
    let trimmed = first.trim_start();
    let marker = ["// ", "# ", "-- ", "* "].into_iter().find(|marker| trimmed.starts_with(marker)).unwrap_or("");
    let prefix = format!("{indentation}{marker}");
    let words = source
        .lines()
        .flat_map(|line| {
            let line = line.trim_start();
            line.strip_prefix(marker).unwrap_or(line).split_whitespace()
        })
        .collect::<Vec<_>>();
    if words.is_empty() {
        return source.to_owned();
    }
    let mut output = prefix.clone();
    let mut column = prefix.chars().count();
    for word in words {
        let separator = usize::from(column > prefix.chars().count());
        if column + separator + word.chars().count() > width && column > prefix.chars().count() {
            output.push('\n');
            output.push_str(&prefix);
            output.push_str(word);
            column = prefix.chars().count() + word.chars().count();
        } else {
            if separator == 1 {
                output.push(' ');
                column += 1;
            }
            output.push_str(word);
            column += word.chars().count();
        }
    }
    if source.ends_with('\n') {
        output.push('\n');
    }
    output
}

pub(super) fn formatter_invocation(path: &Path) -> Option<FormatterInvocation> {
    let language = bundled_language_id(path)?;
    if matches!(language, "c" | "cpp") {
        let mut arguments = vec!["--mode=c".to_owned(), "--suffix=none".to_owned()];
        if let Some(options) = find_upward(path, ".astylerc") {
            arguments.push(format!("--options={}", options.display()));
        }
        return Some(FormatterInvocation { program: "astyle".to_owned(), arguments });
    }
    let profile = ToolProfile::for_language(FORMATTER_PROFILES, language)?;
    Some(FormatterInvocation { program: profile.program.to_owned(), arguments: profile.arguments(path) })
}

pub(super) fn find_upward(path: &Path, name: &str) -> Option<PathBuf> {
    let mut directory = path.parent();
    while let Some(current) = directory {
        let candidate = current.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        directory = current.parent();
    }
    None
}

pub(super) fn executable_exists(program: &str) -> bool {
    if program.contains(std::path::MAIN_SEPARATOR) {
        return Path::new(program).is_file();
    }
    env::var_os("PATH").is_some_and(|path| env::split_paths(&path).any(|directory| directory.join(program).is_file()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DiagnosticInvocation {
    pub(super) program: String,
    pub(super) arguments: Vec<String>,
    pub(super) directory: PathBuf,
}

pub(super) fn diagnostic_invocation(path: &Path, workspace_root: &Path) -> Option<DiagnosticInvocation> {
    let language = bundled_language_id(path)?;
    let profile = ToolProfile::for_language(DIAGNOSTIC_PROFILES, language)?;
    let directory = profile.parent_directory.then(|| path.parent()).flatten().unwrap_or(workspace_root).to_path_buf();
    Some(DiagnosticInvocation { program: profile.program.to_owned(), arguments: profile.arguments(path), directory })
}

pub(super) fn parse_diagnostic_line(line: &str, directory: &Path) -> Option<DiagnosticEntry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let mut parts = line.splitn(4, ':');
    let raw_path = parts.next()?.trim();
    let line_number = parts.next()?.trim().parse::<usize>().ok()?;
    let column = parts.next().and_then(|value| value.trim().parse::<usize>().ok()).unwrap_or(1);
    let message = parts.next().unwrap_or_default().trim();
    let path = PathBuf::from(raw_path);
    let path = if path.is_absolute() { path } else { directory.join(path) };
    let lowercase = message.to_ascii_lowercase();
    let severity = if lowercase.contains("error") {
        DiagnosticSeverity::Error
    } else if lowercase.contains("warning") || lowercase.contains("warn") {
        DiagnosticSeverity::Warning
    } else if lowercase.contains("hint") {
        DiagnosticSeverity::Hint
    } else {
        DiagnosticSeverity::Information
    };
    Some(DiagnosticEntry { path, line: line_number.max(1), column: column.max(1), severity, message: message.to_owned() })
}
