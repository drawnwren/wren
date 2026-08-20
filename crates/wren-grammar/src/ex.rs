use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExAddress {
    Current,
    Last,
    Line(usize),
    Mark(char),
    SearchForward(Box<str>),
    SearchBackward(Box<str>),
    Offset { base: Box<ExAddress>, delta: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExRange {
    pub start: ExAddress,
    pub end: Option<ExAddress>,
    pub semicolon: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SubstituteFlags {
    pub global: bool,
    pub confirm: bool,
    pub case_sensitive: Option<bool>,
    pub print: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferAction {
    Next,
    Previous,
    First,
    Last,
    Delete,
    Select,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabAction {
    New,
    Next,
    Previous,
    First,
    Last,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExCommand {
    Goto { address: ExAddress },
    Substitute { range: Option<ExRange>, pattern: Box<str>, replacement: Box<str>, flags: SubstituteFlags },
    SubstituteRepeat { range: Option<ExRange>, use_search_pattern: bool, flags: Option<SubstituteFlags> },
    Global { range: Option<ExRange>, invert: bool, pattern: Box<str>, command: Box<ExCommand> },
    Normal { range: Option<ExRange>, bang: bool, keys: Box<str> },
    Write { range: Option<ExRange>, all: bool, bang: bool, path: Option<Box<str>> },
    WriteQuit { bang: bool, path: Option<Box<str>> },
    Quit { all: bool, bang: bool },
    Edit { bang: bool, path: Option<Box<str>> },
    Buffer { action: BufferAction, bang: bool, target: Option<Box<str>> },
    Split { vertical: bool, path: Option<Box<str>> },
    Close { bang: bool },
    Tab { action: TabAction, path: Option<Box<str>> },
    Marks { names: Box<str> },
    Registers { names: Box<str> },
    Grep { pattern: Box<str>, paths: Vec<Box<str>> },
    Cdo { command: Box<ExCommand> },
    Undo,
    Redo,
    Echo { expression: Box<str> },
    NoHighlight,
    Help { topic: Option<Box<str>> },
    Messages,
    ConvertUtf8,
    Terminal { program: Option<Box<str>>, arguments: Vec<Box<str>> },
    Make { program: Box<str>, arguments: Vec<Box<str>> },
    Format { program: Box<str>, arguments: Vec<Box<str>> },
    Find { query: Box<str> },
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ExError {
    #[error("empty Ex command")]
    Empty,
    #[error("invalid Ex address near byte {offset}")]
    InvalidAddress { offset: usize },
    #[error("unterminated Ex delimiter {delimiter:?}")]
    UnterminatedDelimiter { delimiter: char },
    #[error("invalid substitution flag {flag:?}")]
    InvalidSubstituteFlag { flag: char },
    #[error("missing argument for :{command}")]
    MissingArgument { command: Box<str> },
    #[error("not an editor command: {0}")]
    UnknownCommand(Box<str>),
    #[error("invalid nested command in :{command}: {source}")]
    Nested {
        command: Box<str>,
        #[source]
        source: Box<ExError>,
    },
}

pub fn parse_ex(input: &str) -> Result<ExCommand, ExError> {
    let input = input.trim();
    let input = input.strip_prefix(':').unwrap_or(input);
    if input.is_empty() {
        return Err(ExError::Empty);
    }
    let (range, rest) = parse_range(input)?;
    let rest = rest.trim_start();
    if rest.is_empty() {
        return range.map(|range| ExCommand::Goto { address: range.end.unwrap_or(range.start) }).ok_or(ExError::Empty);
    }
    if let Some(argument) = rest.strip_prefix('&') {
        return parse_substitute_repeat(range, false, argument);
    }
    if let Some(argument) = rest.strip_prefix('~') {
        return parse_substitute_repeat(range, true, argument);
    }
    let name_end = rest.char_indices().find(|(_, character)| !character.is_ascii_alphanumeric()).map_or(rest.len(), |(index, _)| index);
    if name_end == 0 {
        return Err(ExError::UnknownCommand(rest.into()));
    }
    let name = &rest[..name_end];
    let mut tail = &rest[name_end..];
    let bang = tail.starts_with('!');
    if bang {
        tail = &tail[1..];
    }
    let argument = tail.trim_start();

    parse_named_ex(range, name, bang, argument)
}

type ExCommandParser = fn(Option<ExRange>, &str, bool, &str) -> Result<ExCommand, ExError>;

struct ExCommandSpec {
    names: &'static [&'static str],
    completion: &'static str,
    parse: ExCommandParser,
}

const EX_COMMANDS: &[ExCommandSpec] = &[
    ExCommandSpec { names: &["bn", "bnext"], completion: "bnext", parse: |_, _, bang, argument| Ok(buffer(BufferAction::Next, bang, argument)) },
    ExCommandSpec { names: &["bp", "bprevious"], completion: "bprevious", parse: |_, _, bang, argument| Ok(buffer(BufferAction::Previous, bang, argument)) },
    ExCommandSpec { names: &["bf", "bfirst"], completion: "bfirst", parse: |_, _, bang, argument| Ok(buffer(BufferAction::First, bang, argument)) },
    ExCommandSpec { names: &["bl", "blast"], completion: "blast", parse: |_, _, bang, argument| Ok(buffer(BufferAction::Last, bang, argument)) },
    ExCommandSpec { names: &["bd", "bdelete"], completion: "bdelete", parse: |_, _, bang, argument| Ok(buffer(BufferAction::Delete, bang, argument)) },
    ExCommandSpec { names: &["b", "buffer"], completion: "buffer", parse: |_, _, bang, argument| Ok(buffer(BufferAction::Select, bang, argument)) },
    ExCommandSpec { names: &["cdo"], completion: "cdo", parse: |_, name, _, argument| parse_cdo(argument, name) },
    ExCommandSpec { names: &["clo", "close"], completion: "close", parse: |_, _, bang, _| Ok(ExCommand::Close { bang }) },
    ExCommandSpec { names: &["convertutf8"], completion: "convertutf8", parse: |_, _, _, _| Ok(ExCommand::ConvertUtf8) },
    ExCommandSpec { names: &["debuglog"], completion: "debuglog", parse: |_, _, _, _| Ok(ExCommand::Messages) },
    ExCommandSpec { names: &["echo"], completion: "echo", parse: |_, _, _, argument| Ok(ExCommand::Echo { expression: argument.into() }) },
    ExCommandSpec { names: &["e", "edit"], completion: "edit", parse: |_, _, bang, argument| Ok(ExCommand::Edit { bang, path: optional_argument(argument) }) },
    ExCommandSpec { names: &["find"], completion: "find", parse: |_, _, _, argument| Ok(ExCommand::Find { query: argument.into() }) },
    ExCommandSpec { names: &["format"], completion: "format", parse: |_, name, _, argument| parse_required_process(argument, name, ProcessCommand::Format) },
    ExCommandSpec { names: &["g", "global"], completion: "global", parse: |range, name, _, argument| parse_global(range, false, argument, name) },
    ExCommandSpec { names: &["grep", "vimgrep"], completion: "grep", parse: |_, name, _, argument| parse_grep(argument, name) },
    ExCommandSpec { names: &["h", "help"], completion: "help", parse: |_, _, _, argument| Ok(ExCommand::Help { topic: optional_argument(argument) }) },
    ExCommandSpec { names: &["make"], completion: "make", parse: |_, name, _, argument| parse_required_process(argument, name, ProcessCommand::Make) },
    ExCommandSpec { names: &["marks"], completion: "marks", parse: |_, _, _, argument| Ok(ExCommand::Marks { names: argument.into() }) },
    ExCommandSpec { names: &["mes", "messages"], completion: "messages", parse: |_, _, _, _| Ok(ExCommand::Messages) },
    ExCommandSpec { names: &["noh", "nohlsearch"], completion: "nohlsearch", parse: |_, _, _, _| Ok(ExCommand::NoHighlight) },
    ExCommandSpec {
        names: &["norm", "normal"],
        completion: "normal",
        parse: |range, _, bang, argument| Ok(ExCommand::Normal { range, bang, keys: argument.into() }),
    },
    ExCommandSpec { names: &["q", "quit"], completion: "quit", parse: |_, _, bang, _| Ok(ExCommand::Quit { all: false, bang }) },
    ExCommandSpec { names: &["reg", "registers"], completion: "registers", parse: |_, _, _, argument| Ok(ExCommand::Registers { names: argument.into() }) },
    ExCommandSpec { names: &["redo"], completion: "redo", parse: |_, _, _, _| Ok(ExCommand::Redo) },
    ExCommandSpec {
        names: &["sp", "split"],
        completion: "split",
        parse: |_, _, _, argument| Ok(ExCommand::Split { vertical: false, path: optional_argument(argument) }),
    },
    ExCommandSpec { names: &["s", "substitute"], completion: "substitute", parse: |range, _, _, argument| parse_substitute(range, argument) },
    ExCommandSpec { names: &["tabclose"], completion: "tabclose", parse: |_, _, _, argument| Ok(tab(TabAction::Close, argument)) },
    ExCommandSpec { names: &["tabfirst"], completion: "tabfirst", parse: |_, _, _, argument| Ok(tab(TabAction::First, argument)) },
    ExCommandSpec { names: &["tablast"], completion: "tablast", parse: |_, _, _, argument| Ok(tab(TabAction::Last, argument)) },
    ExCommandSpec { names: &["tabnew", "tabe", "tabedit"], completion: "tabnew", parse: |_, _, _, argument| Ok(tab(TabAction::New, argument)) },
    ExCommandSpec { names: &["tabn", "tabnext"], completion: "tabnext", parse: |_, _, _, argument| Ok(tab(TabAction::Next, argument)) },
    ExCommandSpec { names: &["tabp", "tabprevious"], completion: "tabprevious", parse: |_, _, _, argument| Ok(tab(TabAction::Previous, argument)) },
    ExCommandSpec {
        names: &["term", "terminal"],
        completion: "terminal",
        parse: |_, _, _, argument| {
            let mut words = argument.split_whitespace();
            Ok(ExCommand::Terminal { program: words.next().map(Into::into), arguments: words.map(Into::into).collect() })
        },
    },
    ExCommandSpec { names: &["u", "undo"], completion: "undo", parse: |_, _, _, _| Ok(ExCommand::Undo) },
    ExCommandSpec { names: &["v", "vglobal"], completion: "vglobal", parse: |range, name, _, argument| parse_global(range, true, argument, name) },
    ExCommandSpec {
        names: &["vs", "vsplit"],
        completion: "vsplit",
        parse: |_, _, _, argument| Ok(ExCommand::Split { vertical: true, path: optional_argument(argument) }),
    },
    ExCommandSpec {
        names: &["wa", "wall"],
        completion: "wall",
        parse: |range, _, bang, argument| Ok(ExCommand::Write { range, all: true, bang, path: optional_argument(argument) }),
    },
    ExCommandSpec {
        names: &["w", "write"],
        completion: "write",
        parse: |range, _, bang, argument| Ok(ExCommand::Write { range, all: false, bang, path: optional_argument(argument) }),
    },
    ExCommandSpec {
        names: &["wq", "x", "xit"],
        completion: "wq",
        parse: |_, _, bang, argument| Ok(ExCommand::WriteQuit { bang, path: optional_argument(argument) }),
    },
    ExCommandSpec { names: &["qa", "qall"], completion: "qall", parse: |_, _, bang, _| Ok(ExCommand::Quit { all: true, bang }) },
];

pub fn ex_command_completions() -> impl Iterator<Item = &'static str> {
    EX_COMMANDS.iter().map(|command| command.completion)
}

fn parse_named_ex(range: Option<ExRange>, name: &str, bang: bool, argument: &str) -> Result<ExCommand, ExError> {
    EX_COMMANDS
        .iter()
        .find(|command| command.names.contains(&name))
        .ok_or_else(|| ExError::UnknownCommand(name.into()))
        .and_then(|command| (command.parse)(range, name, bang, argument))
}

fn parse_cdo(argument: &str, name: &str) -> Result<ExCommand, ExError> {
    if argument.is_empty() {
        return Err(ExError::MissingArgument { command: name.into() });
    }
    parse_ex(argument)
        .map(|command| ExCommand::Cdo { command: Box::new(command) })
        .map_err(|source| ExError::Nested { command: name.into(), source: Box::new(source) })
}

enum ProcessCommand {
    Make,
    Format,
}

fn parse_required_process(argument: &str, name: &str, command: ProcessCommand) -> Result<ExCommand, ExError> {
    let mut words = argument.split_whitespace();
    let program = words.next().ok_or_else(|| ExError::MissingArgument { command: name.into() })?;
    let arguments = words.map(Into::into).collect();
    Ok(match command {
        ProcessCommand::Make => ExCommand::Make { program: program.into(), arguments },
        ProcessCommand::Format => ExCommand::Format { program: program.into(), arguments },
    })
}

fn parse_range(input: &str) -> Result<(Option<ExRange>, &str), ExError> {
    if let Some(rest) = input.strip_prefix('%') {
        return Ok((Some(ExRange { start: ExAddress::Line(1), end: Some(ExAddress::Last), semicolon: false }), rest));
    }
    let Some((start, consumed)) = parse_address(input, 0)? else {
        return Ok((None, input));
    };
    let mut rest = &input[consumed..];
    let mut end = None;
    let mut semicolon = false;
    if let Some(separator) = rest.chars().next()
        && matches!(separator, ',' | ';')
    {
        semicolon = separator == ';';
        rest = &rest[separator.len_utf8()..];
        let (parsed, consumed) = parse_address(rest, input.len() - rest.len())?.unwrap_or((ExAddress::Last, 0));
        end = Some(parsed);
        rest = &rest[consumed..];
    }
    Ok((Some(ExRange { start, end, semicolon }), rest))
}

fn parse_address(input: &str, offset: usize) -> Result<Option<(ExAddress, usize)>, ExError> {
    let Some(first) = input.chars().next() else {
        return Ok(None);
    };
    let (mut address, mut consumed) = match first {
        '.' => (ExAddress::Current, 1),
        '$' => (ExAddress::Last, 1),
        '\'' => {
            let Some(mark) = input[1..].chars().next() else {
                return Err(ExError::InvalidAddress { offset });
            };
            (ExAddress::Mark(mark), 1 + mark.len_utf8())
        }
        '/' | '?' => {
            let (pattern, length) = delimited(input, first)?;
            let address = if first == '/' { ExAddress::SearchForward(pattern.into()) } else { ExAddress::SearchBackward(pattern.into()) };
            (address, length)
        }
        character if character.is_ascii_digit() => {
            let digits = input
                .char_indices()
                .take_while(|(_, character)| character.is_ascii_digit())
                .map(|(index, character)| index + character.len_utf8())
                .last()
                .unwrap_or(0);
            let line = input[..digits].parse::<usize>().map_err(|_| ExError::InvalidAddress { offset })?;
            (ExAddress::Line(line), digits)
        }
        '+' | '-' => (ExAddress::Current, 0),
        _ => return Ok(None),
    };
    loop {
        let rest = &input[consumed..];
        let Some(sign) = rest.chars().next() else {
            break;
        };
        if !matches!(sign, '+' | '-') {
            break;
        }
        consumed += 1;
        let digit_length = input[consumed..]
            .char_indices()
            .take_while(|(_, character)| character.is_ascii_digit())
            .map(|(index, character)| index + character.len_utf8())
            .last()
            .unwrap_or(0);
        let amount = if digit_length == 0 {
            1
        } else {
            input[consumed..consumed + digit_length].parse::<i64>().map_err(|_| ExError::InvalidAddress { offset: offset + consumed })?
        };
        consumed += digit_length;
        address = ExAddress::Offset { base: Box::new(address), delta: if sign == '-' { -amount } else { amount } };
    }
    Ok(Some((address, consumed)))
}

fn parse_substitute(range: Option<ExRange>, argument: &str) -> Result<ExCommand, ExError> {
    if argument.is_empty() {
        return Ok(ExCommand::SubstituteRepeat { range, use_search_pattern: false, flags: None });
    }
    let delimiter = argument.chars().next().ok_or_else(|| ExError::MissingArgument { command: "substitute".into() })?;
    let (pattern, pattern_bytes) = delimited(argument, delimiter)?;
    let replacement_input = &argument[pattern_bytes..];
    let (replacement, replacement_bytes) = delimited_tail(replacement_input, delimiter)?;
    let flags_input = replacement_input[replacement_bytes..].trim();
    let flags = parse_substitute_flags(flags_input)?;
    Ok(ExCommand::Substitute { range, pattern: pattern.into(), replacement: replacement.into(), flags })
}

fn parse_substitute_repeat(range: Option<ExRange>, use_search_pattern: bool, argument: &str) -> Result<ExCommand, ExError> {
    let argument = argument.trim();
    Ok(ExCommand::SubstituteRepeat { range, use_search_pattern, flags: (!argument.is_empty()).then(|| parse_substitute_flags(argument)).transpose()? })
}

fn parse_substitute_flags(input: &str) -> Result<SubstituteFlags, ExError> {
    let mut flags = SubstituteFlags::default();
    for flag in input.chars() {
        match flag {
            'g' => flags.global = true,
            'c' => flags.confirm = true,
            'i' => flags.case_sensitive = Some(false),
            'I' => flags.case_sensitive = Some(true),
            'p' => flags.print = true,
            character if character.is_whitespace() => {}
            flag => return Err(ExError::InvalidSubstituteFlag { flag }),
        }
    }
    Ok(flags)
}

fn parse_global(range: Option<ExRange>, invert: bool, argument: &str, name: &str) -> Result<ExCommand, ExError> {
    let delimiter = argument.chars().next().ok_or_else(|| ExError::MissingArgument { command: name.into() })?;
    let (pattern, consumed) = delimited(argument, delimiter)?;
    let nested = argument[consumed..].trim_start();
    if nested.is_empty() {
        return Err(ExError::MissingArgument { command: name.into() });
    }
    parse_ex(nested)
        .map(|command| ExCommand::Global { range, invert, pattern: pattern.into(), command: Box::new(command) })
        .map_err(|source| ExError::Nested { command: name.into(), source: Box::new(source) })
}

fn parse_grep(argument: &str, name: &str) -> Result<ExCommand, ExError> {
    if argument.is_empty() {
        return Err(ExError::MissingArgument { command: name.into() });
    }
    let (pattern, rest) = if let Some(delimiter) = argument.chars().next().filter(|c| !c.is_alphanumeric()) {
        let (pattern, consumed) = delimited(argument, delimiter)?;
        (pattern, argument[consumed..].trim())
    } else {
        argument.split_once(char::is_whitespace).map_or_else(|| (argument.to_owned(), ""), |(pattern, rest)| (pattern.to_owned(), rest.trim()))
    };
    Ok(ExCommand::Grep { pattern: pattern.into(), paths: rest.split_whitespace().map(Into::into).collect() })
}

fn delimited(input: &str, delimiter: char) -> Result<(String, usize), ExError> {
    if !input.starts_with(delimiter) {
        return Err(ExError::UnterminatedDelimiter { delimiter });
    }
    let (value, consumed) = delimited_tail(&input[delimiter.len_utf8()..], delimiter)?;
    Ok((value, delimiter.len_utf8() + consumed))
}

fn delimited_tail(input: &str, delimiter: char) -> Result<(String, usize), ExError> {
    let mut value = String::new();
    let mut escaped = false;
    for (index, character) in input.char_indices() {
        match (escaped, character) {
            (true, character) => {
                if character != delimiter {
                    value.push('\\');
                }
                value.push(character);
                escaped = false;
            }
            (false, '\\') => escaped = true,
            (false, character) if character == delimiter => return Ok((value, index + character.len_utf8())),
            (false, character) => value.push(character),
        }
    }
    Err(ExError::UnterminatedDelimiter { delimiter })
}

fn optional_argument(argument: &str) -> Option<Box<str>> {
    (!argument.is_empty()).then(|| argument.into())
}

fn buffer(action: BufferAction, bang: bool, argument: &str) -> ExCommand {
    ExCommand::Buffer { action, bang, target: optional_argument(argument) }
}

fn tab(action: TabAction, argument: &str) -> ExCommand {
    ExCommand::Tab { action, path: optional_argument(argument) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_addresses_ranges_and_offsets() {
        assert_eq!(
            parse_ex("'a+2,$-1normal! dd").expect("parse"),
            ExCommand::Normal {
                range: Some(ExRange {
                    start: ExAddress::Offset { base: Box::new(ExAddress::Mark('a')), delta: 2 },
                    end: Some(ExAddress::Offset { base: Box::new(ExAddress::Last), delta: -1 }),
                    semicolon: false,
                }),
                bang: true,
                keys: "dd".into(),
            }
        );
        assert_eq!(parse_ex("42").expect("goto"), ExCommand::Goto { address: ExAddress::Line(42) });
    }

    #[test]
    fn parses_substitute_escapes_and_flags() {
        assert_eq!(
            parse_ex(r"%s/one\/two/ONE\/TWO/gip").expect("substitute"),
            ExCommand::Substitute {
                range: Some(ExRange { start: ExAddress::Line(1), end: Some(ExAddress::Last), semicolon: false }),
                pattern: "one/two".into(),
                replacement: "ONE/TWO".into(),
                flags: SubstituteFlags { global: true, confirm: false, case_sensitive: Some(false), print: true },
            }
        );
        assert_eq!(parse_ex("s").expect("repeat substitute"), ExCommand::SubstituteRepeat { range: None, use_search_pattern: false, flags: None });
        assert_eq!(
            parse_ex("%&gI").expect("ampersand repeat"),
            ExCommand::SubstituteRepeat {
                range: Some(ExRange { start: ExAddress::Line(1), end: Some(ExAddress::Last), semicolon: false }),
                use_search_pattern: false,
                flags: Some(SubstituteFlags { global: true, confirm: false, case_sensitive: Some(true), print: false }),
            }
        );
        assert!(matches!(parse_ex("~"), Ok(ExCommand::SubstituteRepeat { use_search_pattern: true, .. })));
        let ExCommand::Substitute { pattern, replacement, .. } = parse_ex(r"s/\\/\\/").expect("literal backslashes") else {
            panic!("expected substitute");
        };
        assert_eq!(pattern.as_ref(), r"\\");
        assert_eq!(replacement.as_ref(), r"\\");
    }

    #[test]
    fn parses_global_nested_normal_and_cdo() {
        assert!(matches!(
            parse_ex("g/TODO/normal A!").expect("global"),
            ExCommand::Global {
                command,
                invert: false,
                ..
            } if matches!(*command, ExCommand::Normal { .. })
        ));
        assert!(matches!(
            parse_ex("cdo s/a/b/g").expect("cdo"),
            ExCommand::Cdo { command }
                if matches!(*command, ExCommand::Substitute { .. })
        ));
    }

    #[test]
    fn covers_published_view_and_workspace_commands() {
        let commands = [
            "wa",
            "edit! src/lib.rs",
            "bnext",
            "bdelete! 3",
            "split file",
            "vsplit",
            "close!",
            "tabnew x",
            "tabnext",
            "marks ab",
            "registers az",
            "grep /needle/ src",
            "qall!",
        ];
        for command in commands {
            assert!(parse_ex(command).is_ok(), "failed to parse {command}");
        }
    }

    #[test]
    fn every_completion_is_owned_by_the_parser_table() {
        let completions = ex_command_completions().collect::<Vec<_>>();
        assert_eq!(completions.len(), 41);
        assert_eq!(completions.iter().copied().collect::<std::collections::BTreeSet<_>>().len(), completions.len());
        for completion in completions {
            assert!(!matches!(parse_ex(completion), Err(ExError::UnknownCommand(_))), "completion is not parseable: {completion}");
        }
    }

    #[test]
    fn parses_explicit_terminal_and_task_commands_without_implicit_shell_expansion() {
        assert_eq!(parse_ex("terminal /bin/sh -l").expect("terminal"), ExCommand::Terminal { program: Some("/bin/sh".into()), arguments: vec!["-l".into()] });
        assert_eq!(parse_ex("make cargo check").expect("make"), ExCommand::Make { program: "cargo".into(), arguments: vec!["check".into()] });
        assert!(matches!(parse_ex("make"), Err(ExError::MissingArgument { .. })));
        assert_eq!(
            parse_ex("format /usr/bin/tr a-z A-Z").expect("format"),
            ExCommand::Format { program: "/usr/bin/tr".into(), arguments: vec!["a-z".into(), "A-Z".into()] }
        );
        assert_eq!(parse_ex("find src main").expect("find"), ExCommand::Find { query: "src main".into() });
    }

    #[test]
    fn parses_message_and_debug_log_commands() {
        assert_eq!(parse_ex("messages").expect("messages"), ExCommand::Messages);
        assert_eq!(parse_ex("debuglog").expect("debug log"), ExCommand::Messages);
    }

    #[test]
    fn rejects_incomplete_or_unknown_commands() {
        assert!(matches!(parse_ex("wat"), Err(ExError::UnknownCommand(_))));
        assert!(matches!(parse_ex("s/foo/bar/x"), Err(ExError::InvalidSubstituteFlag { flag: 'x' })));
        assert!(matches!(parse_ex("g/foo/"), Err(ExError::MissingArgument { .. })));
    }
}
