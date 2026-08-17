#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::VecDeque;
use std::io::{self, Write};
use std::time::Duration;

use base64::Engine as _;
use termina::escape::csi::{
    Csi, Cursor, DecPrivateMode, DecPrivateModeCode, Edit, EraseInDisplay, EraseInLine, Keyboard,
    KittyKeyboardFlags, Mode, Sgr, SgrAttributes, SgrModifiers,
};
use termina::escape::osc::{Osc, Selection};
use termina::event::{
    KeyCode as TermKeyCode, KeyEventKind, Modifiers as TermModifiers, MouseButton, MouseEventKind,
};
use termina::style::{ColorSpec, RgbColor};
use termina::{Event, OneBased, Parser, PlatformTerminal, Terminal};
use thiserror::Error;
use wren_view::{CellColor, CellStyle, TerminalPatch};

pub trait TerminalBackend {
    type Error;

    fn submit(&mut self, patch: &[TerminalPatch]) -> Result<(), Self::Error>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalKeyCode {
    Char(char),
    Escape,
    Enter,
    Tab,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalKey {
    pub code: TerminalKeyCode,
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub super_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalInput {
    Key(TerminalKey),
    Paste(String),
    Resized {
        columns: usize,
        rows: usize,
    },
    MouseScroll {
        /// Negative scrolls toward earlier lines; positive toward later lines.
        lines: isize,
        column: usize,
        row: usize,
    },
    MouseClick {
        column: usize,
        row: usize,
    },
    MouseDrag {
        column: usize,
        row: usize,
    },
    MouseRelease {
        column: usize,
        row: usize,
    },
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardSelection {
    Clipboard,
    Primary,
}

#[derive(Debug, Error)]
pub enum TerminalError {
    #[error("terminal initialization failed: {0}")]
    Initialize(String),
    #[error("terminal rendering failed: {0}")]
    Render(String),
    #[error("terminal input failed: {0}")]
    Input(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Interactive Termina terminal. Wren owns application protocol modes while
/// Termina owns the platform raw/cooked mode and input parser.
pub struct SystemTerminalBackend {
    terminal: PlatformTerminal,
    true_color: bool,
    render_buffer: Vec<u8>,
    style_cache: Vec<(CellStyle, Box<[u8]>)>,
    deferred_parser: Parser,
    deferred_input: VecDeque<TerminalInput>,
}

impl SystemTerminalBackend {
    pub fn open() -> Result<Self, TerminalError> {
        let mut terminal = PlatformTerminal::new()
            .map_err(|error| TerminalError::Initialize(error.to_string()))?;
        terminal
            .enter_raw_mode()
            .map_err(|error| TerminalError::Initialize(error.to_string()))?;
        if let Err(error) = initialize_terminal(&mut terminal) {
            let _ = cleanup_terminal(&mut terminal);
            let _ = terminal.enter_cooked_mode();
            return Err(TerminalError::Initialize(error.to_string()));
        }
        terminal.set_panic_hook(|output| {
            let _ = cleanup_terminal(output);
            let _ = output.flush();
        });
        Ok(Self {
            terminal,
            true_color: supports_true_color(),
            render_buffer: Vec::with_capacity(64 * 1024),
            style_cache: Vec::new(),
            deferred_parser: Parser::default(),
            deferred_input: VecDeque::new(),
        })
    }

    pub fn size(&mut self) -> Result<(usize, usize), TerminalError> {
        self.terminal
            .get_dimensions()
            .map(|size| (usize::from(size.cols.max(1)), usize::from(size.rows.max(1))))
            .map_err(|error| TerminalError::Input(error.to_string()))
    }

    pub fn poll_input(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Option<TerminalInput>, TerminalError> {
        if let Some(input) = self.deferred_input.pop_front() {
            return Ok(Some(input));
        }
        let available = self
            .terminal
            .poll(|_| true, timeout)
            .map_err(|error| TerminalError::Input(error.to_string()))?;
        if !available {
            return Ok(None);
        }
        self.terminal
            .read(|_| true)
            .map(map_input)
            .map(Some)
            .map_err(|error| TerminalError::Input(error.to_string()))
    }

    /// Reads a clipboard selection from the client terminal using OSC 52.
    /// This deliberately operates on `/dev/tty`: stdout may be forwarded over
    /// SSH, while the terminal device remains the client-services boundary.
    /// Bytes typed during the bounded query are parsed and replayed afterward.
    #[cfg(unix)]
    pub fn paste_osc52(
        &mut self,
        selection: ClipboardSelection,
        timeout: Duration,
    ) -> Result<Option<String>, TerminalError> {
        use std::fs::OpenOptions;
        use std::io::{ErrorKind, Read};
        use std::thread;
        use std::time::Instant;

        const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024 / 3 + 64;
        let Ok(mut tty) = OpenOptions::new().read(true).open("/dev/tty") else {
            return Ok(None);
        };
        let flags = rustix::fs::fcntl_getfl(&tty)
            .map_err(|error| TerminalError::Input(error.to_string()))?;
        rustix::fs::fcntl_setfl(&tty, flags | rustix::fs::OFlags::NONBLOCK)
            .map_err(|error| TerminalError::Input(error.to_string()))?;
        let terminal_selection = match selection {
            ClipboardSelection::Clipboard => Selection::CLIPBOARD,
            ClipboardSelection::Primary => Selection::PRIMARY,
        };
        write!(self.terminal, "{}", Osc::QuerySelection(terminal_selection))?;
        self.terminal.flush()?;

        let started = Instant::now();
        let mut response = Vec::with_capacity(1024);
        let mut chunk = [0_u8; 4096];
        while started.elapsed() < timeout {
            match tty.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    response.extend_from_slice(&chunk[..read]);
                    if response.len() > MAX_RESPONSE_BYTES {
                        return Err(TerminalError::Input(
                            "OSC 52 clipboard response exceeds 1 MiB".to_owned(),
                        ));
                    }
                    if let Some((text, range)) = decode_osc52_response(&response, selection)? {
                        self.defer_terminal_bytes(&response[..range.start]);
                        self.defer_terminal_bytes(&response[range.end..]);
                        return Ok(Some(text));
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(TerminalError::Io(error)),
            }
        }
        self.defer_terminal_bytes(&response);
        Ok(None)
    }

    #[cfg(not(unix))]
    pub fn paste_osc52(
        &mut self,
        _selection: ClipboardSelection,
        _timeout: Duration,
    ) -> Result<Option<String>, TerminalError> {
        Ok(None)
    }

    fn defer_terminal_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.deferred_parser.parse(bytes, false);
        while let Some(event) = self.deferred_parser.pop() {
            let input = map_input(event);
            if input != TerminalInput::Ignored {
                self.deferred_input.push_back(input);
            }
        }
    }
}

fn decode_osc52_response(
    bytes: &[u8],
    wanted: ClipboardSelection,
) -> Result<Option<(String, std::ops::Range<usize>)>, TerminalError> {
    const PREFIX: &[u8] = b"\x1b]52;";
    let Some(start) = bytes
        .windows(PREFIX.len())
        .position(|window| window == PREFIX)
    else {
        return Ok(None);
    };
    let fields = start + PREFIX.len();
    let Some(selection_end) = bytes[fields..].iter().position(|byte| *byte == b';') else {
        return Ok(None);
    };
    let selection_end = fields + selection_end;
    let wanted = match wanted {
        ClipboardSelection::Clipboard => b'c',
        ClipboardSelection::Primary => b'p',
    };
    if !bytes[fields..selection_end].contains(&wanted) {
        return Ok(None);
    }
    let payload_start = selection_end + 1;
    let terminator = bytes[payload_start..]
        .iter()
        .position(|byte| *byte == 0x07)
        .map(|offset| (payload_start + offset, 1))
        .or_else(|| {
            bytes[payload_start..]
                .windows(2)
                .position(|window| window == b"\x1b\\")
                .map(|offset| (payload_start + offset, 2))
        });
    let Some((payload_end, terminator_len)) = terminator else {
        return Ok(None);
    };
    let payload = &bytes[payload_start..payload_end];
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(payload))
        .map_err(|error| {
            TerminalError::Input(format!("invalid OSC 52 clipboard response: {error}"))
        })?;
    let text = String::from_utf8(decoded)
        .map_err(|error| TerminalError::Input(format!("OSC 52 clipboard is not UTF-8: {error}")))?;
    Ok(Some((text, start..payload_end + terminator_len)))
}

impl Drop for SystemTerminalBackend {
    fn drop(&mut self) {
        let _ = cleanup_terminal(&mut self.terminal);
        let _ = self.terminal.flush();
        let _ = self.terminal.enter_cooked_mode();
    }
}

impl TerminalBackend for SystemTerminalBackend {
    type Error = TerminalError;

    fn submit(&mut self, patch: &[TerminalPatch]) -> Result<(), Self::Error> {
        self.render_buffer.clear();
        render_patches(
            &mut self.render_buffer,
            patch,
            self.true_color,
            &mut self.style_cache,
        )?;
        self.terminal.write_all(&self.render_buffer)?;
        self.terminal
            .flush()
            .map_err(|error| TerminalError::Render(error.to_string()))
    }
}

/// Termina-backed renderer. Termina types do not escape this crate.
pub struct TerminaBackend<W> {
    writer: W,
    true_color: bool,
    render_buffer: Vec<u8>,
    style_cache: Vec<(CellStyle, Box<[u8]>)>,
}

impl<W: Write> TerminaBackend<W> {
    pub fn new(writer: W, _width: usize, _height: usize) -> Result<Self, TerminalError> {
        Ok(Self {
            writer,
            true_color: supports_true_color(),
            render_buffer: Vec::with_capacity(64 * 1024),
            style_cache: Vec::new(),
        })
    }

    #[must_use]
    pub fn into_inner(self) -> W {
        self.writer
    }

    pub fn resize(&mut self, _width: usize, _height: usize) {}

    /// Copies through the client terminal, so workspace-side code never gains
    /// access to the local clipboard. OSC 52 is bounded to avoid turning a
    /// register into an unbounded terminal escape.
    pub fn copy_osc52(
        &mut self,
        selection: ClipboardSelection,
        text: &str,
    ) -> Result<(), TerminalError> {
        const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
        if text.len() > MAX_CLIPBOARD_BYTES {
            return Err(TerminalError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "clipboard register exceeds the 1 MiB OSC 52 limit",
            )));
        }
        let selection = match selection {
            ClipboardSelection::Clipboard => Selection::CLIPBOARD,
            ClipboardSelection::Primary => Selection::PRIMARY,
        };
        write!(self.writer, "{}", Osc::SetSelection(selection, text))?;
        self.writer.flush()?;
        Ok(())
    }
}

impl<W: Write> TerminalBackend for TerminaBackend<W> {
    type Error = TerminalError;

    fn submit(&mut self, patch: &[TerminalPatch]) -> Result<(), Self::Error> {
        self.render_buffer.clear();
        render_patches(
            &mut self.render_buffer,
            patch,
            self.true_color,
            &mut self.style_cache,
        )?;
        self.writer.write_all(&self.render_buffer)?;
        self.writer
            .flush()
            .map_err(|error| TerminalError::Render(error.to_string()))
    }
}

fn dec_mode(code: DecPrivateModeCode) -> DecPrivateMode {
    DecPrivateMode::Code(code)
}

fn initialize_terminal(output: &mut impl Write) -> io::Result<()> {
    // Button-event tracking reports motion only while a button is held. That
    // is enough for selection without the unbounded idle pointer stream from
    // any-event mode 1003.
    write!(
        output,
        "{}{}{}{}{}{}",
        Csi::Mode(Mode::SetDecPrivateMode(dec_mode(
            DecPrivateModeCode::ClearAndEnableAlternateScreen
        ))),
        Csi::Mode(Mode::SetDecPrivateMode(dec_mode(
            DecPrivateModeCode::BracketedPaste
        ))),
        Csi::Mode(Mode::SetDecPrivateMode(dec_mode(
            DecPrivateModeCode::MouseTracking
        ))),
        Csi::Mode(Mode::SetDecPrivateMode(dec_mode(
            DecPrivateModeCode::ButtonEventMouse
        ))),
        Csi::Mode(Mode::SetDecPrivateMode(dec_mode(
            DecPrivateModeCode::SGRMouse
        ))),
        Csi::Keyboard(Keyboard::PushFlags(
            KittyKeyboardFlags::DISAMBIGUATE_ESCAPE_CODES
                | KittyKeyboardFlags::REPORT_ALTERNATE_KEYS
        )),
    )?;
    output.flush()
}

fn cleanup_terminal(output: &mut impl Write) -> io::Result<()> {
    write!(
        output,
        "{}{}{}{}{}{}{}{}",
        Csi::Sgr(Sgr::Reset),
        Csi::Mode(Mode::SetDecPrivateMode(dec_mode(
            DecPrivateModeCode::ShowCursor
        ))),
        Csi::Keyboard(Keyboard::PopFlags(1)),
        Csi::Mode(Mode::ResetDecPrivateMode(dec_mode(
            DecPrivateModeCode::SGRMouse
        ))),
        Csi::Mode(Mode::ResetDecPrivateMode(dec_mode(
            DecPrivateModeCode::ButtonEventMouse
        ))),
        Csi::Mode(Mode::ResetDecPrivateMode(dec_mode(
            DecPrivateModeCode::MouseTracking
        ))),
        Csi::Mode(Mode::ResetDecPrivateMode(dec_mode(
            DecPrivateModeCode::BracketedPaste
        ))),
        Csi::Mode(Mode::ResetDecPrivateMode(dec_mode(
            DecPrivateModeCode::ClearAndEnableAlternateScreen
        ))),
    )
}

fn supports_true_color() -> bool {
    let term = std::env::var("TERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let term_program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let color_term = std::env::var("COLORTERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(color_term.as_str(), "truecolor" | "24bit")
        || term.contains("ghostty")
        || term_program.contains("ghostty")
}

fn render_patches(
    output: &mut impl Write,
    patch: &[TerminalPatch],
    true_color: bool,
    style_cache: &mut Vec<(CellStyle, Box<[u8]>)>,
) -> Result<(), TerminalError> {
    let mut active_style = None;
    for change in patch {
        match change {
            TerminalPatch::Clear => {
                write!(
                    output,
                    "{}{}",
                    Csi::Sgr(Sgr::Reset),
                    Csi::Edit(Edit::EraseInDisplay(EraseInDisplay::EraseDisplay))
                )?;
                active_style = None;
            }
            TerminalPatch::ClearToEndOfLine(style) => {
                write_style_if_changed(output, *style, true_color, &mut active_style, style_cache)?;
                write!(
                    output,
                    "{}",
                    Csi::Edit(Edit::EraseInLine(EraseInLine::EraseToEndOfLine))
                )?;
            }
            TerminalPatch::MoveTo { column, row } => write!(
                output,
                "{}",
                Csi::Cursor(Cursor::Position {
                    line: terminal_coordinate(*row)?,
                    col: terminal_coordinate(*column)?,
                })
            )?,
            TerminalPatch::SetStyle(style) => {
                write_style_if_changed(output, *style, true_color, &mut active_style, style_cache)?
            }
            TerminalPatch::Put(cell) => output.write_all(cell.grapheme.as_bytes())?,
            TerminalPatch::PutRow(row) => {
                for cell in &row.cells {
                    write_style_if_changed(
                        output,
                        cell.style,
                        true_color,
                        &mut active_style,
                        style_cache,
                    )?;
                    output.write_all(cell.grapheme.as_bytes())?;
                }
            }
            TerminalPatch::ShowCursor(visible) => {
                let mode = dec_mode(DecPrivateModeCode::ShowCursor);
                write!(
                    output,
                    "{}",
                    Csi::Mode(if *visible {
                        Mode::SetDecPrivateMode(mode)
                    } else {
                        Mode::ResetDecPrivateMode(mode)
                    })
                )?;
            }
        }
    }
    Ok(())
}

fn write_style_if_changed(
    output: &mut impl Write,
    style: CellStyle,
    true_color: bool,
    active_style: &mut Option<CellStyle>,
    cache: &mut Vec<(CellStyle, Box<[u8]>)>,
) -> io::Result<()> {
    if *active_style == Some(style) {
        return Ok(());
    }
    *active_style = Some(style);
    if let Some((_, bytes)) = cache.iter().find(|(cached, _)| *cached == style) {
        return output.write_all(bytes);
    }
    let mut bytes = Vec::with_capacity(48);
    write_style(&mut bytes, style, true_color)?;
    output.write_all(&bytes)?;
    cache.push((style, bytes.into_boxed_slice()));
    Ok(())
}

fn terminal_coordinate(zero_based: usize) -> Result<OneBased, TerminalError> {
    let zero_based = u16::try_from(zero_based).map_err(|_| {
        TerminalError::Render(format!("terminal coordinate {zero_based} exceeds u16"))
    })?;
    if zero_based == u16::MAX {
        return Err(TerminalError::Render(
            "terminal coordinate 65535 cannot be represented as one-based".to_owned(),
        ));
    }
    Ok(OneBased::from_zero_based(zero_based))
}

fn write_style(output: &mut impl Write, style: CellStyle, true_color: bool) -> io::Result<()> {
    let mut attributes = SgrAttributes {
        foreground: style.foreground.map(|color| color_spec(color, true_color)),
        background: style.background.map(|color| color_spec(color, true_color)),
        modifiers: SgrModifiers::RESET,
        ..SgrAttributes::default()
    };
    if style.bold {
        attributes.modifiers |= SgrModifiers::INTENSITY_BOLD;
    }
    if style.underline {
        attributes.modifiers |= SgrModifiers::UNDERLINE_SINGLE;
    }
    if style.italic {
        attributes.modifiers |= SgrModifiers::ITALIC;
    }
    if style.strikethrough {
        attributes.modifiers |= SgrModifiers::STRIKE_THROUGH;
    }
    if style.reverse {
        attributes.modifiers |= SgrModifiers::REVERSE;
    }
    write!(output, "{}", Csi::Sgr(Sgr::Attributes(attributes)))
}

fn color_spec(color: CellColor, true_color: bool) -> ColorSpec {
    match color {
        CellColor::Palette(index) => ColorSpec::PaletteIndex(index),
        CellColor::Rgb(color) if true_color => {
            ColorSpec::from(RgbColor::new(color.red, color.green, color.blue))
        }
        CellColor::Rgb(color) => {
            ColorSpec::PaletteIndex(rgb_to_ansi256(color.red, color.green, color.blue))
        }
    }
}

fn rgb_to_ansi256(red: u8, green: u8, blue: u8) -> u8 {
    let cube = |component: u8| -> u8 {
        if component < 48 {
            0
        } else if component < 115 {
            1
        } else {
            ((u16::from(component) - 35) / 40).min(5) as u8
        }
    };
    let red_cube = cube(red);
    let green_cube = cube(green);
    let blue_cube = cube(blue);
    let cube_index = 16 + 36 * red_cube + 6 * green_cube + blue_cube;
    let cube_level = |value: u8| -> i32 {
        if value == 0 {
            0
        } else {
            55 + 40 * i32::from(value)
        }
    };
    let cube_distance = (i32::from(red) - cube_level(red_cube)).pow(2)
        + (i32::from(green) - cube_level(green_cube)).pow(2)
        + (i32::from(blue) - cube_level(blue_cube)).pow(2);
    let average = (u16::from(red) + u16::from(green) + u16::from(blue)) / 3;
    let gray_index = ((i32::from(average) - 8 + 5) / 10).clamp(0, 23) as u8;
    let gray_level = 8 + 10 * i32::from(gray_index);
    let gray_distance = (i32::from(red) - gray_level).pow(2)
        + (i32::from(green) - gray_level).pow(2)
        + (i32::from(blue) - gray_level).pow(2);
    if gray_distance < cube_distance {
        232 + gray_index
    } else {
        cube_index
    }
}

fn map_input(input: Event) -> TerminalInput {
    match input {
        Event::Key(event) if event.kind != KeyEventKind::Release => {
            let Some((code, forced_shift)) = map_key_code(event.code) else {
                return TerminalInput::Ignored;
            };
            let printable = matches!(code, TerminalKeyCode::Char(_));
            TerminalInput::Key(TerminalKey {
                code,
                shift: forced_shift
                    || (!printable && event.modifiers.contains(TermModifiers::SHIFT)),
                control: event.modifiers.contains(TermModifiers::CONTROL),
                alt: event.modifiers.contains(TermModifiers::ALT),
                super_key: event.modifiers.contains(TermModifiers::SUPER),
            })
        }
        Event::Key(_) => TerminalInput::Ignored,
        Event::Paste(text) => TerminalInput::Paste(text),
        Event::WindowResized(size) => TerminalInput::Resized {
            columns: usize::from(size.cols.max(1)),
            rows: usize::from(size.rows.max(1)),
        },
        Event::Mouse(event) => match event.kind {
            MouseEventKind::Down(MouseButton::Left) => TerminalInput::MouseClick {
                column: usize::from(event.column),
                row: usize::from(event.row),
            },
            MouseEventKind::Drag(MouseButton::Left) => TerminalInput::MouseDrag {
                column: usize::from(event.column),
                row: usize::from(event.row),
            },
            MouseEventKind::Up(MouseButton::Left) => TerminalInput::MouseRelease {
                column: usize::from(event.column),
                row: usize::from(event.row),
            },
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => TerminalInput::MouseScroll {
                lines: if event.kind == MouseEventKind::ScrollUp {
                    -3
                } else {
                    3
                },
                column: usize::from(event.column),
                row: usize::from(event.row),
            },
            _ => TerminalInput::Ignored,
        },
        Event::FocusIn | Event::FocusOut | Event::Csi(_) | Event::Osc(_) | Event::Dcs(_) => {
            TerminalInput::Ignored
        }
    }
}

fn map_key_code(code: TermKeyCode) -> Option<(TerminalKeyCode, bool)> {
    Some(match code {
        TermKeyCode::Char(character) => (TerminalKeyCode::Char(character), false),
        TermKeyCode::Escape => (TerminalKeyCode::Escape, false),
        TermKeyCode::Enter => (TerminalKeyCode::Enter, false),
        TermKeyCode::Tab => (TerminalKeyCode::Tab, false),
        TermKeyCode::BackTab => (TerminalKeyCode::Tab, true),
        TermKeyCode::Backspace => (TerminalKeyCode::Backspace, false),
        TermKeyCode::Delete => (TerminalKeyCode::Delete, false),
        TermKeyCode::Insert => (TerminalKeyCode::Insert, false),
        TermKeyCode::Home => (TerminalKeyCode::Home, false),
        TermKeyCode::End => (TerminalKeyCode::End, false),
        TermKeyCode::PageUp => (TerminalKeyCode::PageUp, false),
        TermKeyCode::PageDown => (TerminalKeyCode::PageDown, false),
        TermKeyCode::Left => (TerminalKeyCode::Left, false),
        TermKeyCode::Right => (TerminalKeyCode::Right, false),
        TermKeyCode::Up => (TerminalKeyCode::Up, false),
        TermKeyCode::Down => (TerminalKeyCode::Down, false),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct CountingSink {
        writes: usize,
        flushes: usize,
        bytes: Vec<u8>,
    }

    impl Write for CountingSink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    fn parse_one(bytes: &[u8]) -> TerminalInput {
        let mut parser = Parser::default();
        parser.parse(bytes, false);
        map_input(parser.pop().expect("parsed event"))
    }

    #[test]
    fn termina_submits_each_complete_patch_set_with_one_writer_call() {
        let mut backend = TerminaBackend::new(CountingSink::default(), 80, 24).expect("backend");
        backend
            .submit(&[
                TerminalPatch::MoveTo { column: 2, row: 3 },
                TerminalPatch::SetStyle(CellStyle::default()),
                TerminalPatch::ShowCursor(true),
            ])
            .expect("submit patches");
        let sink = backend.into_inner();
        assert_eq!(sink.writes, 1);
        assert_eq!(sink.flushes, 1);
        assert!(!sink.bytes.is_empty());
    }

    #[test]
    fn osc52_copy_is_local_bounded_and_base64_encoded() {
        let mut output = Vec::new();
        {
            let mut backend = TerminaBackend::new(&mut output, 80, 24).expect("backend");
            backend
                .copy_osc52(ClipboardSelection::Clipboard, "wren β")
                .expect("clipboard copy");
        }
        assert_eq!(output, b"\x1b]52;c;d3JlbiDOsg==\x1b\\");
    }

    #[test]
    fn termina_mouse_wheel_maps_to_three_editor_lines() {
        assert_eq!(
            parse_one(b"\x1b[<64;15;10M"),
            TerminalInput::MouseScroll {
                lines: -3,
                column: 14,
                row: 9,
            }
        );
        assert_eq!(
            parse_one(b"\x1b[<65;4;5M"),
            TerminalInput::MouseScroll {
                lines: 3,
                column: 3,
                row: 4,
            }
        );
    }

    #[test]
    fn termina_left_click_maps_to_zero_based_editor_coordinates() {
        assert_eq!(
            parse_one(b"\x1b[<0;15;10M"),
            TerminalInput::MouseClick { column: 14, row: 9 }
        );
        assert_eq!(
            parse_one(b"\x1b[<32;17;11M"),
            TerminalInput::MouseDrag {
                column: 16,
                row: 10
            }
        );
        assert_eq!(
            parse_one(b"\x1b[<0;17;11m"),
            TerminalInput::MouseRelease {
                column: 16,
                row: 10
            }
        );
    }

    #[test]
    fn osc52_primary_copy_targets_the_star_register_selection() {
        let mut output = Vec::new();
        {
            let mut backend = TerminaBackend::new(&mut output, 80, 24).expect("backend");
            backend
                .copy_osc52(ClipboardSelection::Primary, "primary")
                .expect("primary clipboard copy");
        }
        assert_eq!(output, b"\x1b]52;p;cHJpbWFyeQ==\x1b\\");
    }

    #[test]
    fn osc52_clipboard_responses_decode_bel_st_and_wrapped_streams() {
        assert_eq!(
            decode_osc52_response(
                b"\x1b]52;c;d3JlbiDOsg==\x1b\\",
                ClipboardSelection::Clipboard
            )
            .expect("valid response")
            .map(|(text, _)| text),
            Some("wren β".to_owned())
        );
        assert_eq!(
            decode_osc52_response(
                b"prefix\x1b]52;p;cHJpbWFyeQ==\x07suffix",
                ClipboardSelection::Primary
            )
            .expect("valid primary response"),
            Some(("primary".to_owned(), 6..26))
        );
        assert_eq!(
            decode_osc52_response(
                b"\x1b]52;p;cHJpbWFyeQ==\x1b\\",
                ClipboardSelection::Clipboard
            )
            .expect("different selection"),
            None
        );
    }

    #[test]
    fn termina_mouse_wheel_bursts_never_degrade_into_key_input() {
        let mut parser = Parser::default();
        let bytes = b"\x1b[<65;4;5M".repeat(2_048);
        parser.parse(&bytes, false);
        let mut count = 0;
        while let Some(event) = parser.pop() {
            assert_eq!(
                map_input(event),
                TerminalInput::MouseScroll {
                    lines: 3,
                    column: 3,
                    row: 4,
                }
            );
            count += 1;
        }
        assert_eq!(count, 2_048);
    }

    #[test]
    fn control_d_is_a_key_in_raw_and_enhanced_protocols() {
        let expected = TerminalInput::Key(TerminalKey {
            code: TerminalKeyCode::Char('d'),
            shift: false,
            control: true,
            alt: false,
            super_key: false,
        });
        assert_eq!(parse_one(b"\x04"), expected);
        assert_eq!(parse_one(b"\x1b[100;5u"), expected);
    }

    #[test]
    fn slash_and_question_mark_survive_raw_and_kitty_keyboard_input() {
        let slash = TerminalInput::Key(TerminalKey {
            code: TerminalKeyCode::Char('/'),
            shift: false,
            control: false,
            alt: false,
            super_key: false,
        });
        let question = TerminalInput::Key(TerminalKey {
            code: TerminalKeyCode::Char('?'),
            shift: false,
            control: false,
            alt: false,
            super_key: false,
        });
        assert_eq!(parse_one(b"/"), slash);
        assert_eq!(parse_one(b"\x1b[47u"), slash);
        assert_eq!(parse_one(b"\x1b[47:63;2u"), question);
    }

    #[test]
    fn bracketed_paste_and_shifted_k_keep_editor_meaning() {
        assert_eq!(
            parse_one(b"\x1b[200~fn main() {}\x1b[201~"),
            TerminalInput::Paste("fn main() {}".to_owned())
        );
        let hover_key = TerminalInput::Key(TerminalKey {
            code: TerminalKeyCode::Char('K'),
            shift: false,
            control: false,
            alt: false,
            super_key: false,
        });
        assert_eq!(parse_one(b"K"), hover_key);
        assert_eq!(parse_one(b"\x1b[107:75;2u"), hover_key);
    }

    #[test]
    fn application_modes_are_typed_and_balanced() {
        let mut output = Vec::new();
        initialize_terminal(&mut output).expect("initialize");
        cleanup_terminal(&mut output).expect("cleanup");
        assert_eq!(
            output,
            b"\x1b[?1049h\x1b[?2004h\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[>5u\x1b[m\x1b[?25h\x1b[<1u\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[?1049l"
        );
    }

    #[test]
    fn renderer_uses_typed_cursor_clear_and_visibility_sequences() {
        let mut output = Vec::new();
        render_patches(
            &mut output,
            &[
                TerminalPatch::Clear,
                TerminalPatch::MoveTo { column: 4, row: 2 },
                TerminalPatch::ShowCursor(false),
                TerminalPatch::ShowCursor(true),
            ],
            true,
            &mut Vec::new(),
        )
        .expect("render");
        assert_eq!(output, b"\x1b[m\x1b[2J\x1b[3;5H\x1b[?25l\x1b[?25h");
    }
}
