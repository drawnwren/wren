use super::*;

pub(super) fn git_root_for(path: &Path) -> Result<PathBuf> {
    let directory = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
    let output = Command::new("git").current_dir(directory).args(["rev-parse", "--show-toplevel"]).output().context("locate Git root")?;
    if !output.status.success() {
        bail!("not inside a Git repository");
    }
    let root = String::from_utf8(output.stdout).context("Git root is not UTF-8")?;
    Ok(PathBuf::from(root.trim()))
}

pub(super) fn git_ex_program(arguments: &[Box<str>]) -> &'static str {
    if arguments.is_empty() { "lazygit" } else { "git" }
}

pub(super) fn terminal_cell_color(color: TerminalColor, default: RgbColor) -> CellColor {
    match color {
        TerminalColor::Default => CellColor::Rgb(default),
        TerminalColor::Palette(index) => CellColor::Palette(index),
        TerminalColor::Rgb(red, green, blue) => CellColor::rgb(red, green, blue),
    }
}

pub(super) fn git_branch_for(path: &Path) -> Option<String> {
    let root = git_root_for(path).ok()?;
    let output = Command::new("git").current_dir(root).args(["symbolic-ref", "--quiet", "--short", "HEAD"]).output().ok()?;
    if !output.status.success() {
        return Some("HEAD".to_owned());
    }
    let branch = String::from_utf8(output.stdout).ok()?;
    Some(branch.trim().to_owned())
}

pub(super) fn git_index_contents(root: &Path, relative: &Path) -> Result<String> {
    let output =
        Command::new("git").current_dir(root).arg("show").arg(format!(":{}", relative.to_string_lossy())).output().context("read file from Git index")?;
    if output.status.success() {
        return String::from_utf8(output.stdout).context("Git index contents are not UTF-8");
    }
    if git_path_tracked(root, relative)? {
        bail!("read Git index: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::new())
}

pub(super) fn git_path_tracked(root: &Path, relative: &Path) -> Result<bool> {
    Ok(Command::new("git")
        .current_dir(root)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(relative)
        .output()
        .context("check whether Git tracks file")?
        .status
        .success())
}

pub(super) fn make_git_patch(root: &Path, relative: &Path, before: &str, after: &str) -> Result<Vec<u8>> {
    if before == after {
        return Ok(Vec::new());
    }
    let mut old = tempfile::NamedTempFile::new().context("create old Git hunk input")?;
    let mut new = tempfile::NamedTempFile::new().context("create new Git hunk input")?;
    old.write_all(before.as_bytes())?;
    new.write_all(after.as_bytes())?;
    let output = Command::new("git")
        .current_dir(root)
        .args(["diff", "--no-index", "--no-color", "--unified=0", "--"])
        .arg(old.path())
        .arg(new.path())
        .output()
        .context("compute Git-compatible buffer patch")?;
    if !output.status.success() && output.status.code() != Some(1) {
        bail!("git diff: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let relative = relative.to_string_lossy();
    let diff = String::from_utf8(output.stdout).context("git diff returned non-UTF-8")?;
    let mut rewritten = String::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            rewritten.push_str(&format!("diff --git a/{relative} b/{relative}\n"));
        } else if line.starts_with("--- ") {
            rewritten.push_str(&format!("--- a/{relative}\n"));
        } else if line.starts_with("+++ ") {
            rewritten.push_str(&format!("+++ b/{relative}\n"));
        } else {
            rewritten.push_str(line);
            rewritten.push('\n');
        }
    }
    Ok(rewritten.into_bytes())
}

pub(super) fn select_git_hunk(patch: &[u8], cursor_line: usize, selected_lines: Option<&Range<usize>>) -> Result<Vec<u8>> {
    let patch = std::str::from_utf8(patch).context("Git patch is not UTF-8")?;
    if patch.is_empty() {
        bail!("buffer has no Git changes");
    }
    let lines = patch.lines().collect::<Vec<_>>();
    let header_end = lines.iter().position(|line| line.starts_with("@@")).ok_or_else(|| anyhow!("Git patch contains no hunks"))?;
    let mut selected = None;
    let mut index = header_end;
    while index < lines.len() {
        let end = lines[index + 1..].iter().position(|line| line.starts_with("@@")).map_or(lines.len(), |offset| index + 1 + offset);
        let range = parse_git_after_range(lines[index])?;
        let matches = selected_lines.map_or_else(
            || {
                let effective_end = range.end.max(range.start.saturating_add(1));
                range.start <= cursor_line && cursor_line < effective_end
            },
            |selection| {
                let effective_end = range.end.max(range.start.saturating_add(1));
                range.start < selection.end && selection.start < effective_end
            },
        );
        if matches {
            selected = Some(index..end);
            break;
        }
        index = end;
    }
    let selected = selected.ok_or_else(|| anyhow!("cursor is not in a changed Git hunk"))?;
    let mut output = String::new();
    for line in lines[..header_end].iter().chain(lines[selected].iter()) {
        output.push_str(line);
        output.push('\n');
    }
    Ok(output.into_bytes())
}

pub(super) fn parse_git_after_range(header: &str) -> Result<Range<usize>> {
    let after =
        header.split_whitespace().find(|field| field.starts_with('+')).ok_or_else(|| anyhow!("invalid Git hunk header {header:?}"))?.trim_start_matches('+');
    let mut values = after.split(',');
    let start = values.next().and_then(|value| value.parse::<usize>().ok()).ok_or_else(|| anyhow!("invalid Git hunk range {after:?}"))?;
    let count = values.next().and_then(|value| value.parse::<usize>().ok()).unwrap_or(1);
    Ok(start..start.saturating_add(count))
}

pub(super) fn byte_range_of_lines(text: &str, lines: Range<usize>) -> Range<usize> {
    fn line_byte(text: &str, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        text.match_indices('\n').nth(line - 1).map_or(text.len(), |(byte, _)| byte + 1)
    }
    line_byte(text, lines.start)..line_byte(text, lines.end)
}

pub(super) fn git_apply_patch(root: &Path, patch: &[u8], cached: bool, reverse: bool) -> Result<()> {
    if patch.is_empty() {
        bail!("Git patch is empty");
    }
    let mut command = Command::new("git");
    command.current_dir(root).args(["apply", "--unidiff-zero", "--whitespace=nowarn"]);
    if cached {
        command.arg("--cached");
    }
    if reverse {
        command.arg("--reverse");
    }
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("start git apply")?;
    child.stdin.take().ok_or_else(|| anyhow!("git apply stdin unavailable"))?.write_all(patch)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("git apply: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

pub(super) fn url_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}
