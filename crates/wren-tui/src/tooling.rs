use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FormatterInvocation {
    pub(super) program: String,
    pub(super) arguments: Vec<String>,
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
        return IndentStyle {
            expand_tabs: false,
            width: 2,
        };
    }
    let width = widths
        .into_iter()
        .filter(|width| *width <= 8)
        .reduce(greatest_common_divisor)
        .filter(|width| *width > 1)
        .unwrap_or(2)
        .clamp(2, 8);
    IndentStyle {
        expand_tabs: true,
        width,
    }
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
    let marker = ["// ", "# ", "-- ", "* "]
        .into_iter()
        .find(|marker| trimmed.starts_with(marker))
        .unwrap_or("");
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
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let extension = extension.as_str();
    let filename = path.to_string_lossy().into_owned();
    let (program, arguments) = match extension {
        "rs" => ("rustfmt", vec!["--emit=stdout".to_owned()]),
        "py" => (
            "ruff",
            vec![
                "format".to_owned(),
                "--stdin-filename".to_owned(),
                filename,
                "-".to_owned(),
            ],
        ),
        "go" => ("gofmt", Vec::new()),
        "tf" | "tfvars" => ("tofu", vec!["fmt".to_owned(), "-".to_owned()]),
        "nix" => ("nixfmt", Vec::new()),
        "hs" | "lhs" => ("fourmolu", vec!["--stdin-input-file".to_owned(), filename]),
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "msg" => {
            let mut arguments = vec!["--mode=c".to_owned(), "--suffix=none".to_owned()];
            if let Some(options) = find_upward(path, ".astylerc") {
                arguments.push(format!("--options={}", options.display()));
            }
            ("astyle", arguments)
        }
        _ => return None,
    };
    Some(FormatterInvocation {
        program: program.to_owned(),
        arguments,
    })
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
    env::var_os("PATH").is_some_and(|path| {
        env::split_paths(&path).any(|directory| directory.join(program).is_file())
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DiagnosticInvocation {
    pub(super) program: String,
    pub(super) arguments: Vec<String>,
    pub(super) directory: PathBuf,
}

pub(super) fn diagnostic_invocation(
    path: &Path,
    workspace_root: &Path,
) -> Option<DiagnosticInvocation> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let extension = extension.as_str();
    let file = path.to_string_lossy().into_owned();
    let (program, arguments, directory) = match extension {
        "rs" => (
            "cargo",
            vec![
                "clippy".to_owned(),
                "--quiet".to_owned(),
                "--message-format=short".to_owned(),
            ],
            workspace_root.to_path_buf(),
        ),
        "py" => (
            "ruff",
            vec![
                "check".to_owned(),
                "--output-format=concise".to_owned(),
                file,
            ],
            workspace_root.to_path_buf(),
        ),
        "js" | "jsx" | "ts" | "tsx" => (
            "pnpm",
            vec![
                "exec".to_owned(),
                "tsc".to_owned(),
                "--noEmit".to_owned(),
                "--pretty".to_owned(),
                "false".to_owned(),
            ],
            workspace_root.to_path_buf(),
        ),
        "go" => (
            "go",
            vec!["vet".to_owned(), "./...".to_owned()],
            workspace_root.to_path_buf(),
        ),
        "tf" | "tfvars" => (
            "tofu",
            vec!["validate".to_owned(), "-no-color".to_owned()],
            path.parent().unwrap_or(workspace_root).to_path_buf(),
        ),
        "nix" => (
            "nix-instantiate",
            vec!["--parse".to_owned(), file],
            workspace_root.to_path_buf(),
        ),
        "hs" | "lhs" => (
            "ghc",
            vec!["-fno-code".to_owned(), file],
            workspace_root.to_path_buf(),
        ),
        "lua" => (
            "luac",
            vec!["-p".to_owned(), file],
            workspace_root.to_path_buf(),
        ),
        "sh" | "bash" | "zsh" => (
            "bash",
            vec!["-n".to_owned(), file],
            workspace_root.to_path_buf(),
        ),
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "msg" => (
            "clang++",
            vec!["-fsyntax-only".to_owned(), file],
            workspace_root.to_path_buf(),
        ),
        _ => return None,
    };
    Some(DiagnosticInvocation {
        program: program.to_owned(),
        arguments,
        directory,
    })
}

pub(super) fn parse_diagnostic_line(line: &str, directory: &Path) -> Option<DiagnosticEntry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let mut parts = line.splitn(4, ':');
    let raw_path = parts.next()?.trim();
    let line_number = parts.next()?.trim().parse::<usize>().ok()?;
    let column = parts
        .next()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(1);
    let message = parts.next().unwrap_or_default().trim();
    let path = PathBuf::from(raw_path);
    let path = if path.is_absolute() {
        path
    } else {
        directory.join(path)
    };
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
    Some(DiagnosticEntry {
        path,
        line: line_number.max(1),
        column: column.max(1),
        severity,
        message: message.to_owned(),
    })
}
