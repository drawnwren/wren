#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::io::{self, Write};
use std::time::Duration;

use termina::escape::csi::{
    Csi, Cursor, DecPrivateMode, DecPrivateModeCode, Edit, EraseInDisplay, EraseInLine, Keyboard,
    KittyKeyboardFlags, Mode, Sgr, SgrAttributes, SgrModifiers,
};
use termina::escape::osc::{Osc, Selection};
use termina::event::{
    KeyCode as TermKeyCode, KeyEventKind, Modifiers as TermModifiers, MouseButton, MouseEventKind,
};
use termina::style::{ColorSpec, RgbColor};
use termina::{Event, OneBased, PlatformTerminal, Terminal};
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
    Ignored,
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
        render_patches(&mut self.terminal, patch, self.true_color)?;
        self.terminal
            .flush()
            .map_err(|error| TerminalError::Render(error.to_string()))
    }
}

/// Termina-backed renderer. Termina types do not escape this crate.
pub struct TerminaBackend<W> {
    writer: W,
    true_color: bool,
}

impl<W: Write> TerminaBackend<W> {
    pub fn new(writer: W, _width: usize, _height: usize) -> Result<Self, TerminalError> {
        Ok(Self {
            writer,
            true_color: supports_true_color(),
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
    pub fn copy_osc52(&mut self, text: &str) -> Result<(), TerminalError> {
        const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
        if text.len() > MAX_CLIPBOARD_BYTES {
            return Err(TerminalError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "clipboard register exceeds the 1 MiB OSC 52 limit",
            )));
        }
        write!(
            self.writer,
            "{}",
            Osc::SetSelection(Selection::CLIPBOARD, text)
        )?;
        self.writer.flush()?;
        Ok(())
    }
}

impl<W: Write> TerminalBackend for TerminaBackend<W> {
    type Error = TerminalError;

    fn submit(&mut self, patch: &[TerminalPatch]) -> Result<(), Self::Error> {
        render_patches(&mut self.writer, patch, self.true_color)?;
        self.writer
            .flush()
            .map_err(|error| TerminalError::Render(error.to_string()))
    }
}

fn dec_mode(code: DecPrivateModeCode) -> DecPrivateMode {
    DecPrivateMode::Code(code)
}

fn initialize_terminal(output: &mut impl Write) -> io::Result<()> {
    // Wren consumes wheel reports but has no pointer-motion bindings. Mode
    // 1000 keeps wheel/button input without the unbounded motion stream that
    // mode 1003 produces while a trackpad or mouse is moving.
    write!(
        output,
        "{}{}{}{}{}",
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
        "{}{}{}{}{}{}{}",
        Csi::Sgr(Sgr::Reset),
        Csi::Mode(Mode::SetDecPrivateMode(dec_mode(
            DecPrivateModeCode::ShowCursor
        ))),
        Csi::Keyboard(Keyboard::PopFlags(1)),
        Csi::Mode(Mode::ResetDecPrivateMode(dec_mode(
            DecPrivateModeCode::SGRMouse
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
) -> Result<(), TerminalError> {
    for change in patch {
        match change {
            TerminalPatch::Clear => write!(
                output,
                "{}{}",
                Csi::Sgr(Sgr::Reset),
                Csi::Edit(Edit::EraseInDisplay(EraseInDisplay::EraseDisplay))
            )?,
            TerminalPatch::ClearToEndOfLine(style) => {
                write_style(output, *style, true_color)?;
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
            TerminalPatch::SetStyle(style) => write_style(output, *style, true_color)?,
            TerminalPatch::Put(cell) => output.write_all(cell.grapheme.as_bytes())?,
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
            let (code, forced_shift) = match map_key_code(event.code) {
                Some(mapped) => mapped,
                None => return TerminalInput::Ignored,
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
        Event::Mouse(event) => {
            if event.kind == MouseEventKind::Down(MouseButton::Left) {
                return TerminalInput::MouseClick {
                    column: usize::from(event.column),
                    row: usize::from(event.row),
                };
            }
            let lines = match event.kind {
                MouseEventKind::ScrollUp => -3,
                MouseEventKind::ScrollDown => 3,
                _ => return TerminalInput::Ignored,
            };
            TerminalInput::MouseScroll {
                lines,
                column: usize::from(event.column),
                row: usize::from(event.row),
            }
        }
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
    use termina::Parser;

    fn parse_one(bytes: &[u8]) -> TerminalInput {
        let mut parser = Parser::default();
        parser.parse(bytes, false);
        map_input(parser.pop().expect("parsed event"))
    }

    #[test]
    fn osc52_copy_is_local_bounded_and_base64_encoded() {
        let mut output = Vec::new();
        {
            let mut backend = TerminaBackend::new(&mut output, 80, 24).expect("backend");
            backend.copy_osc52("wren β").expect("clipboard copy");
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
        assert_eq!(parse_one(b"\x1b[<0;15;10m"), TerminalInput::Ignored);
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
            b"\x1b[?1049h\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[>5u\x1b[m\x1b[?25h\x1b[<1u\x1b[?1006l\x1b[?1000l\x1b[?2004l\x1b[?1049l"
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
        )
        .expect("render");
        assert_eq!(output, b"\x1b[m\x1b[2J\x1b[3;5H\x1b[?25l\x1b[?25h");
    }
}
