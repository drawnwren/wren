#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::time::Duration;

use base64::Engine as _;
use flate2::Compression;
use flate2::write::ZlibEncoder;
use termina::escape::csi::{
    Csi, Cursor, DecPrivateMode, DecPrivateModeCode, Edit, EraseInDisplay, EraseInLine, Keyboard, KittyKeyboardFlags, Mode, Sgr, SgrAttributes, SgrModifiers,
};
use termina::escape::osc::{Osc, Selection};
use termina::event::{KeyCode as TermKeyCode, KeyEventKind, Modifiers as TermModifiers, MouseButton, MouseEventKind};
use termina::style::{ColorSpec, RgbColor};
use termina::{Event, OneBased, Parser, PlatformTerminal, Terminal};
use thiserror::Error;
use wren_types::Modifiers;
pub use wren_types::{KeyCode as TerminalKeyCode, KeyEvent as TerminalKey};
use wren_view::{CellColor, CellStyle, RasterQuad, RasterSource, TerminalPatch};

pub trait TerminalBackend {
    type Error;

    fn submit(&mut self, patch: &[TerminalPatch]) -> Result<(), Self::Error>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalInput {
    Key(TerminalKey),
    Paste(String),
    Resized { columns: usize, rows: usize },
    Mouse { action: MouseAction, column: usize, row: usize },
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    /// Negative scrolls toward earlier lines; positive toward later lines.
    Scroll(isize),
    Click,
    Drag,
    Release,
}

impl TerminalInput {
    #[must_use]
    pub const fn mouse(action: MouseAction, column: usize, row: usize) -> Self {
        Self::Mouse { action, column, row }
    }

    #[must_use]
    pub const fn scroll(lines: isize, column: usize, row: usize) -> Self {
        Self::mouse(MouseAction::Scroll(lines), column, row)
    }

    #[must_use]
    pub const fn click(column: usize, row: usize) -> Self {
        Self::mouse(MouseAction::Click, column, row)
    }

    #[must_use]
    pub const fn drag(column: usize, row: usize) -> Self {
        Self::mouse(MouseAction::Drag, column, row)
    }

    #[must_use]
    pub const fn release(column: usize, row: usize) -> Self {
        Self::mouse(MouseAction::Release, column, row)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardSelection {
    Clipboard,
    Primary,
}

impl ClipboardSelection {
    const fn termina(self) -> Selection {
        match self {
            Self::Clipboard => Selection::CLIPBOARD,
            Self::Primary => Selection::PRIMARY,
        }
    }
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
    renderer: Renderer,
    deferred_parser: Parser,
    deferred_input: VecDeque<TerminalInput>,
}

impl SystemTerminalBackend {
    pub fn open() -> Result<Self, TerminalError> {
        let mut terminal = PlatformTerminal::new().map_err(|error| TerminalError::Initialize(error.to_string()))?;
        terminal.enter_raw_mode().map_err(|error| TerminalError::Initialize(error.to_string()))?;
        if let Err(error) = initialize_terminal(&mut terminal) {
            let _ = cleanup_terminal(&mut terminal);
            let _ = terminal.enter_cooked_mode();
            return Err(TerminalError::Initialize(error.to_string()));
        }
        terminal.set_panic_hook(|output| {
            let _ = cleanup_terminal(output);
            let _ = output.flush();
        });
        Ok(Self { terminal, renderer: Renderer::new(), deferred_parser: Parser::default(), deferred_input: VecDeque::new() })
    }

    pub fn size(&mut self) -> Result<(usize, usize), TerminalError> {
        self.terminal
            .get_dimensions()
            .map(|size| (usize::from(size.cols.max(1)), usize::from(size.rows.max(1))))
            .map_err(|error| TerminalError::Input(error.to_string()))
    }

    pub fn poll_input(&mut self, timeout: Option<Duration>) -> Result<Option<TerminalInput>, TerminalError> {
        if let Some(input) = self.deferred_input.pop_front() {
            return Ok(Some(input));
        }
        let available = self.terminal.poll(|_| true, timeout).map_err(|error| TerminalError::Input(error.to_string()))?;
        if !available {
            return Ok(None);
        }
        self.terminal.read(|_| true).map(map_input).map(Some).map_err(|error| TerminalError::Input(error.to_string()))
    }

    /// Reads a clipboard selection from the client terminal using OSC 52.
    /// This deliberately operates on `/dev/tty`: stdout may be forwarded over
    /// SSH, while the terminal device remains the client-services boundary.
    /// Bytes typed during the bounded query are parsed and replayed afterward.
    #[cfg(unix)]
    pub fn paste_osc52(&mut self, selection: ClipboardSelection, timeout: Duration) -> Result<Option<String>, TerminalError> {
        use std::fs::OpenOptions;
        use std::io::{ErrorKind, Read};
        use std::thread;
        use std::time::Instant;

        const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024 / 3 + 64;
        let Ok(mut tty) = OpenOptions::new().read(true).open("/dev/tty") else {
            return Ok(None);
        };
        let flags = rustix::fs::fcntl_getfl(&tty).map_err(|error| TerminalError::Input(error.to_string()))?;
        rustix::fs::fcntl_setfl(&tty, flags | rustix::fs::OFlags::NONBLOCK).map_err(|error| TerminalError::Input(error.to_string()))?;
        write!(self.terminal, "{}", Osc::QuerySelection(selection.termina()))?;
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
                        return Err(TerminalError::Input("OSC 52 clipboard response exceeds 1 MiB".to_owned()));
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
    pub fn paste_osc52(&mut self, _selection: ClipboardSelection, _timeout: Duration) -> Result<Option<String>, TerminalError> {
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

fn decode_osc52_response(bytes: &[u8], wanted: ClipboardSelection) -> Result<Option<(String, std::ops::Range<usize>)>, TerminalError> {
    const PREFIX: &[u8] = b"\x1b]52;";
    let Some(start) = bytes.windows(PREFIX.len()).position(|window| window == PREFIX) else {
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
        .or_else(|| bytes[payload_start..].windows(2).position(|window| window == b"\x1b\\").map(|offset| (payload_start + offset, 2)));
    let Some((payload_end, terminator_len)) = terminator else {
        return Ok(None);
    };
    let payload = &bytes[payload_start..payload_end];
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(payload))
        .map_err(|error| TerminalError::Input(format!("invalid OSC 52 clipboard response: {error}")))?;
    let text = String::from_utf8(decoded).map_err(|error| TerminalError::Input(format!("OSC 52 clipboard is not UTF-8: {error}")))?;
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
        self.renderer.submit(&mut self.terminal, patch)
    }
}

/// Termina-backed renderer. Termina types do not escape this crate.
pub struct TerminaBackend<W> {
    writer: W,
    renderer: Renderer,
}

impl<W: Write> TerminaBackend<W> {
    pub fn new(writer: W, _width: usize, _height: usize) -> Result<Self, TerminalError> {
        Ok(Self { writer, renderer: Renderer::new() })
    }

    #[must_use]
    pub fn into_inner(self) -> W {
        self.writer
    }

    pub fn resize(&mut self, _width: usize, _height: usize) {}

    /// Copies through the client terminal, so workspace-side code never gains
    /// access to the local clipboard. OSC 52 is bounded to avoid turning a
    /// register into an unbounded terminal escape.
    pub fn copy_osc52(&mut self, selection: ClipboardSelection, text: &str) -> Result<(), TerminalError> {
        const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
        if text.len() > MAX_CLIPBOARD_BYTES {
            return Err(TerminalError::Io(io::Error::new(io::ErrorKind::InvalidInput, "clipboard register exceeds the 1 MiB OSC 52 limit")));
        }
        write!(self.writer, "{}", Osc::SetSelection(selection.termina(), text))?;
        self.writer.flush()?;
        Ok(())
    }
}

impl<W: Write> TerminalBackend for TerminaBackend<W> {
    type Error = TerminalError;

    fn submit(&mut self, patch: &[TerminalPatch]) -> Result<(), Self::Error> {
        self.renderer.submit(&mut self.writer, patch)
    }
}

struct Renderer {
    raster_rows: Option<usize>,
    buffer: Vec<u8>,
    style_cache: StyleCache,
}

impl Renderer {
    fn new() -> Self {
        Self { raster_rows: None, buffer: Vec::with_capacity(64 * 1024), style_cache: StyleCache::default() }
    }

    fn submit(&mut self, writer: &mut impl Write, patch: &[TerminalPatch]) -> Result<(), TerminalError> {
        self.buffer.clear();
        self.buffer.extend_from_slice(SYNCHRONIZED_OUTPUT_BEGIN);
        render_patches(&mut self.buffer, patch, &mut self.raster_rows, &mut self.style_cache)?;
        self.buffer.extend_from_slice(SYNCHRONIZED_OUTPUT_END);
        writer.write_all(&self.buffer)?;
        writer.flush().map_err(|error| TerminalError::Render(error.to_string()))
    }
}

// These invariant encodings are kept out of the formatter hot path. A test
// pins them to Termina's typed DEC mode representation.
const SYNCHRONIZED_OUTPUT_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNCHRONIZED_OUTPUT_END: &[u8] = b"\x1b[?2026l";

fn dec_mode(code: DecPrivateModeCode) -> DecPrivateMode {
    DecPrivateMode::Code(code)
}

const TERMINAL_MODES: [DecPrivateModeCode; 6] = [
    DecPrivateModeCode::ClearAndEnableAlternateScreen,
    DecPrivateModeCode::BracketedPaste,
    DecPrivateModeCode::MouseTracking,
    DecPrivateModeCode::ButtonEventMouse,
    DecPrivateModeCode::SGRMouse,
    DecPrivateModeCode::GraphemeClustering,
];

fn write_terminal_modes(output: &mut impl Write, enabled: bool) -> io::Result<()> {
    if enabled {
        for code in TERMINAL_MODES {
            write!(output, "{}", Csi::Mode(Mode::SetDecPrivateMode(dec_mode(code))))?;
        }
    } else {
        for code in TERMINAL_MODES.into_iter().rev() {
            write!(output, "{}", Csi::Mode(Mode::ResetDecPrivateMode(dec_mode(code))))?;
        }
    }
    Ok(())
}

fn initialize_terminal(output: &mut impl Write) -> io::Result<()> {
    // Button-event tracking reports motion only while a button is held. That
    // is enough for selection without the unbounded idle pointer stream from
    // any-event mode 1003.
    write_terminal_modes(output, true)?;
    write!(output, "{}", Csi::Keyboard(Keyboard::PushFlags(KittyKeyboardFlags::DISAMBIGUATE_ESCAPE_CODES | KittyKeyboardFlags::REPORT_ALTERNATE_KEYS)),)?;
    output.flush()
}

fn cleanup_terminal(output: &mut impl Write) -> io::Result<()> {
    write!(
        output,
        "\x1b_Ga=d,d=I,i={KITTY_RASTER_IMAGE_ID},q=2\x1b\\{}{}{}{}",
        Csi::Sgr(Sgr::Reset),
        Csi::Mode(Mode::ResetDecPrivateMode(dec_mode(DecPrivateModeCode::SynchronizedOutput))),
        Csi::Mode(Mode::SetDecPrivateMode(dec_mode(DecPrivateModeCode::ShowCursor))),
        Csi::Keyboard(Keyboard::PopFlags(1)),
    )?;
    write_terminal_modes(output, false)
}

fn render_patches(
    output: &mut impl Write,
    patch: &[TerminalPatch],
    raster_rows: &mut Option<usize>,
    style_cache: &mut StyleCache,
) -> Result<(), TerminalError> {
    let mut raster_rows_for_submit = *raster_rows;
    for change in patch {
        if let TerminalPatch::SetRasterOverlay(overlay) = change {
            raster_rows_for_submit = overlay.as_ref().map(|overlay| overlay.rows);
        }
    }
    let mut active_style = None;
    let mut current_row = None;
    for change in patch {
        match change {
            TerminalPatch::Clear => {
                write!(output, "{}{}", Csi::Sgr(Sgr::Reset), Csi::Edit(Edit::EraseInDisplay(EraseInDisplay::EraseDisplay)))?;
                active_style = None;
            }
            TerminalPatch::ClearToEndOfLine(style) => {
                if current_row.is_some_and(|row| raster_rows_for_submit.is_some_and(|raster_rows| row < raster_rows)) {
                    continue;
                }
                write_style_if_changed(output, *style, &mut active_style, style_cache)?;
                write!(output, "{}", Csi::Edit(Edit::EraseInLine(EraseInLine::EraseToEndOfLine)))?;
            }
            TerminalPatch::MoveTo { column, row } => {
                current_row = Some(*row);
                write!(output, "{}", Csi::Cursor(Cursor::Position { line: terminal_coordinate(*row)?, col: terminal_coordinate(*column)? }))?;
            }
            TerminalPatch::SetStyle(style) => write_style_if_changed(output, *style, &mut active_style, style_cache)?,
            TerminalPatch::Put(cell) => output.write_all(cell.grapheme.as_bytes())?,
            TerminalPatch::PutRow(row) => {
                if current_row.is_some_and(|row| raster_rows_for_submit.is_some_and(|raster_rows| row < raster_rows)) {
                    continue;
                }
                for cell in &row.cells {
                    write_style_if_changed(output, cell.style, &mut active_style, style_cache)?;
                    output.write_all(cell.grapheme.as_bytes())?;
                }
            }
            TerminalPatch::SetRasterOverlay(overlay) => {
                match overlay {
                    Some(overlay) => write_kitty_raster_overlay(output, overlay)?,
                    None => delete_kitty_raster_overlay(output)?,
                }
                *raster_rows = overlay.as_ref().map(|overlay| overlay.rows);
            }
            TerminalPatch::ShowCursor(visible) => {
                let mode = dec_mode(DecPrivateModeCode::ShowCursor);
                write!(output, "{}", Csi::Mode(if *visible { Mode::SetDecPrivateMode(mode) } else { Mode::ResetDecPrivateMode(mode) }))?;
            }
        }
    }
    Ok(())
}

const KITTY_RASTER_IMAGE_ID: u32 = 0x5752_454e;
const KITTY_RASTER_PLACEMENT_ID: u32 = 1;
const KITTY_PAYLOAD_CHUNK_BYTES: usize = 4096;

fn write_kitty_raster_overlay(output: &mut impl Write, overlay: &wren_view::RasterOverlay) -> Result<(), TerminalError> {
    let expected_bytes = overlay
        .width
        .checked_mul(overlay.height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| TerminalError::Render("raster overlay dimensions overflow".to_owned()))?;
    let (rgb, compressed) = match &overlay.source {
        RasterSource::Rgb(rgb) => {
            if rgb.len() != expected_bytes {
                return Err(TerminalError::Render(format!("raster overlay contains {} bytes, expected {expected_bytes}", rgb.len())));
            }
            (Cow::Borrowed(rgb.as_slice()), false)
        }
        RasterSource::Quads { background, quads } => (Cow::Owned(rasterize_quads(overlay.width, overlay.height, *background, quads)?), true),
    };
    let payload = if compressed {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&rgb)?;
        Cow::Owned(encoder.finish()?)
    } else {
        rgb
    };
    write!(output, "{}", Csi::Cursor(Cursor::Position { line: terminal_coordinate(0)?, col: terminal_coordinate(0)? }))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload.as_ref());
    let mut chunks = encoded.as_bytes().chunks(KITTY_PAYLOAD_CHUNK_BYTES).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = usize::from(chunks.peek().is_some());
        if first {
            let compression = if compressed { ",o=z" } else { "" };
            write!(
                output,
                "\x1b_Ga=T,f=24{compression},s={},v={},i={KITTY_RASTER_IMAGE_ID},p={KITTY_RASTER_PLACEMENT_ID},c={},r={},C=1,z=-1,q=2,m={more};",
                overlay.width, overlay.height, overlay.columns, overlay.rows,
            )?;
            first = false;
        } else {
            write!(output, "\x1b_Gm={more};")?;
        }
        output.write_all(chunk)?;
        output.write_all(b"\x1b\\")?;
    }
    Ok(())
}

fn rasterize_quads(width: usize, height: usize, background: wren_view::RgbColor, quads: &[RasterQuad]) -> Result<Vec<u8>, TerminalError> {
    let byte_count = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| TerminalError::Render("raster overlay dimensions overflow".to_owned()))?;
    let mut rgb = vec![0; byte_count];
    for pixel in rgb.chunks_exact_mut(3) {
        pixel.copy_from_slice(&[background.red, background.green, background.blue]);
    }
    for quad in quads {
        if quad.vertices.iter().flatten().any(|coordinate| !coordinate.is_finite()) {
            return Err(TerminalError::Render("raster overlay contains a non-finite quad".to_owned()));
        }
        let min_x = quad.vertices.iter().map(|point| point[0]).fold(f32::INFINITY, f32::min).floor().clamp(0.0, width as f32) as usize;
        let max_x = quad.vertices.iter().map(|point| point[0]).fold(f32::NEG_INFINITY, f32::max).ceil().clamp(0.0, width as f32) as usize;
        let min_y = quad.vertices.iter().map(|point| point[1]).fold(f32::INFINITY, f32::min).floor().clamp(0.0, height as f32) as usize;
        let max_y = quad.vertices.iter().map(|point| point[1]).fold(f32::NEG_INFINITY, f32::max).ceil().clamp(0.0, height as f32) as usize;
        for row in min_y..max_y {
            for column in min_x..max_x {
                let point = [column as f32 + 0.5, row as f32 + 0.5];
                if !point_in_quad(point, quad.vertices) {
                    continue;
                }
                let offset = (row * width + column) * 3;
                rgb[offset..offset + 3].copy_from_slice(&[quad.color.red, quad.color.green, quad.color.blue]);
            }
        }
    }
    Ok(rgb)
}

#[inline]
fn point_in_quad(point: [f32; 2], vertices: [[f32; 2]; 4]) -> bool {
    let mut has_positive = false;
    let mut has_negative = false;
    for edge in 0..4 {
        let start = vertices[edge];
        let end = vertices[(edge + 1) % 4];
        let cross = (end[0] - start[0]) * (point[1] - start[1]) - (end[1] - start[1]) * (point[0] - start[0]);
        has_positive |= cross > 0.0;
        has_negative |= cross < 0.0;
        if has_positive && has_negative {
            return false;
        }
    }
    true
}

fn delete_kitty_raster_overlay(output: &mut impl Write) -> io::Result<()> {
    write!(output, "\x1b_Ga=d,d=I,i={KITTY_RASTER_IMAGE_ID},q=2\x1b\\")
}

fn write_style_if_changed(output: &mut impl Write, style: CellStyle, active_style: &mut Option<CellStyle>, cache: &mut StyleCache) -> io::Result<()> {
    if *active_style == Some(style) {
        return Ok(());
    }
    *active_style = Some(style);
    let key = style_key(style);
    if let Some(bytes) = cache.entries.get(&key) {
        return output.write_all(bytes);
    }
    let mut bytes = Vec::with_capacity(48);
    write_style(&mut bytes, style)?;
    output.write_all(&bytes)?;
    if cache.entries.len() < STYLE_CACHE_CAPACITY {
        cache.entries.insert(key, bytes.into_boxed_slice());
    }
    Ok(())
}

const STYLE_CACHE_CAPACITY: usize = 65_536;

#[derive(Default)]
struct StyleCache {
    entries: HashMap<u128, Box<[u8]>>,
}

fn style_key(style: CellStyle) -> u128 {
    let flags = u128::from(style.bold)
        | (u128::from(style.italic) << 1)
        | (u128::from(style.underline) << 2)
        | (u128::from(style.strikethrough) << 3)
        | (u128::from(style.reverse) << 4);
    flags | (u128::from(color_key(style.foreground)) << 8) | (u128::from(color_key(style.background)) << 40)
}

const fn color_key(color: Option<CellColor>) -> u32 {
    match color {
        None => 0,
        Some(CellColor::Palette(index)) => 1 | (index as u32) << 2,
        Some(CellColor::Rgb(color)) => 2 | (color.red as u32) << 2 | (color.green as u32) << 10 | (color.blue as u32) << 18,
    }
}

fn terminal_coordinate(zero_based: usize) -> Result<OneBased, TerminalError> {
    let zero_based = u16::try_from(zero_based).map_err(|_| TerminalError::Render(format!("terminal coordinate {zero_based} exceeds u16")))?;
    if zero_based == u16::MAX {
        return Err(TerminalError::Render("terminal coordinate 65535 cannot be represented as one-based".to_owned()));
    }
    Ok(OneBased::from_zero_based(zero_based))
}

fn write_style(output: &mut impl Write, style: CellStyle) -> io::Result<()> {
    let mut attributes = SgrAttributes {
        foreground: style.foreground.map(color_spec),
        background: style.background.map(color_spec),
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

fn color_spec(color: CellColor) -> ColorSpec {
    match color {
        CellColor::Palette(index) => ColorSpec::PaletteIndex(index),
        CellColor::Rgb(color) => ColorSpec::from(RgbColor::new(color.red, color.green, color.blue)),
    }
}

fn map_input(input: Event) -> TerminalInput {
    match input {
        Event::Key(event) if event.kind != KeyEventKind::Release => {
            let Some((code, forced_shift)) = map_key_code(event.code) else {
                return TerminalInput::Ignored;
            };
            let printable = matches!(code, TerminalKeyCode::Char(_));
            let mut modifiers = Modifiers::empty();
            modifiers.set(Modifiers::SHIFT, forced_shift || (!printable && event.modifiers.contains(TermModifiers::SHIFT)));
            modifiers.set(Modifiers::CONTROL, event.modifiers.contains(TermModifiers::CONTROL));
            modifiers.set(Modifiers::ALT, event.modifiers.contains(TermModifiers::ALT));
            modifiers.set(Modifiers::SUPER, event.modifiers.contains(TermModifiers::SUPER));
            TerminalInput::Key(TerminalKey::modified(code, modifiers))
        }
        Event::Paste(text) => TerminalInput::Paste(text),
        Event::WindowResized(size) => TerminalInput::Resized { columns: usize::from(size.cols.max(1)), rows: usize::from(size.rows.max(1)) },
        Event::Mouse(event) => match event.kind {
            MouseEventKind::Down(MouseButton::Left) => TerminalInput::click(usize::from(event.column), usize::from(event.row)),
            MouseEventKind::Drag(MouseButton::Left) => TerminalInput::drag(usize::from(event.column), usize::from(event.row)),
            MouseEventKind::Up(MouseButton::Left) => TerminalInput::release(usize::from(event.column), usize::from(event.row)),
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                TerminalInput::scroll(if event.kind == MouseEventKind::ScrollUp { -3 } else { 3 }, usize::from(event.column), usize::from(event.row))
            }
            _ => TerminalInput::Ignored,
        },
        Event::Key(_) | Event::FocusIn | Event::FocusOut | Event::Csi(_) | Event::Osc(_) | Event::Dcs(_) => TerminalInput::Ignored,
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
            .submit(&[TerminalPatch::MoveTo { column: 2, row: 3 }, TerminalPatch::SetStyle(CellStyle::default()), TerminalPatch::ShowCursor(true)])
            .expect("submit patches");
        let sink = backend.into_inner();
        assert_eq!(sink.writes, 1);
        assert_eq!(sink.flushes, 1);
        assert!(sink.bytes.starts_with(SYNCHRONIZED_OUTPUT_BEGIN));
        assert!(sink.bytes.ends_with(SYNCHRONIZED_OUTPUT_END));
        let synchronized = dec_mode(DecPrivateModeCode::SynchronizedOutput);
        assert_eq!(String::from_utf8_lossy(SYNCHRONIZED_OUTPUT_BEGIN), Csi::Mode(Mode::SetDecPrivateMode(synchronized)).to_string());
        assert_eq!(String::from_utf8_lossy(SYNCHRONIZED_OUTPUT_END), Csi::Mode(Mode::ResetDecPrivateMode(synchronized)).to_string());
    }

    #[test]
    fn osc52_copy_is_local_bounded_and_base64_encoded() {
        let mut output = Vec::new();
        {
            let mut backend = TerminaBackend::new(&mut output, 80, 24).expect("backend");
            backend.copy_osc52(ClipboardSelection::Clipboard, "wren β").expect("clipboard copy");
        }
        assert_eq!(output, b"\x1b]52;c;d3JlbiDOsg==\x1b\\");
    }

    #[test]
    fn termina_mouse_wheel_maps_to_three_editor_lines() {
        assert_eq!(parse_one(b"\x1b[<64;15;10M"), TerminalInput::scroll(-3, 14, 9));
        assert_eq!(parse_one(b"\x1b[<65;4;5M"), TerminalInput::scroll(3, 3, 4));
    }

    #[test]
    fn termina_left_click_maps_to_zero_based_editor_coordinates() {
        assert_eq!(parse_one(b"\x1b[<0;15;10M"), TerminalInput::click(14, 9));
        assert_eq!(parse_one(b"\x1b[<32;17;11M"), TerminalInput::drag(16, 10));
        assert_eq!(parse_one(b"\x1b[<0;17;11m"), TerminalInput::release(16, 10));
    }

    #[test]
    fn osc52_primary_copy_targets_the_star_register_selection() {
        let mut output = Vec::new();
        {
            let mut backend = TerminaBackend::new(&mut output, 80, 24).expect("backend");
            backend.copy_osc52(ClipboardSelection::Primary, "primary").expect("primary clipboard copy");
        }
        assert_eq!(output, b"\x1b]52;p;cHJpbWFyeQ==\x1b\\");
    }

    #[test]
    fn osc52_clipboard_responses_decode_bel_st_and_wrapped_streams() {
        assert_eq!(
            decode_osc52_response(b"\x1b]52;c;d3JlbiDOsg==\x1b\\", ClipboardSelection::Clipboard).expect("valid response").map(|(text, _)| text),
            Some("wren β".to_owned())
        );
        assert_eq!(
            decode_osc52_response(b"prefix\x1b]52;p;cHJpbWFyeQ==\x07suffix", ClipboardSelection::Primary).expect("valid primary response"),
            Some(("primary".to_owned(), 6..26))
        );
        assert_eq!(decode_osc52_response(b"\x1b]52;p;cHJpbWFyeQ==\x1b\\", ClipboardSelection::Clipboard).expect("different selection"), None);
    }

    #[test]
    fn termina_mouse_wheel_bursts_never_degrade_into_key_input() {
        let mut parser = Parser::default();
        let bytes = b"\x1b[<65;4;5M".repeat(2_048);
        parser.parse(&bytes, false);
        let mut count = 0;
        while let Some(event) = parser.pop() {
            assert_eq!(map_input(event), TerminalInput::scroll(3, 3, 4));
            count += 1;
        }
        assert_eq!(count, 2_048);
    }

    #[test]
    fn control_d_is_a_key_in_raw_and_enhanced_protocols() {
        let expected = TerminalInput::Key(TerminalKey::modified(TerminalKeyCode::Char('d'), Modifiers::CONTROL));
        assert_eq!(parse_one(b"\x04"), expected);
        assert_eq!(parse_one(b"\x1b[100;5u"), expected);
    }

    #[test]
    fn slash_and_question_mark_survive_raw_and_kitty_keyboard_input() {
        let slash = TerminalInput::Key(TerminalKey::character('/'));
        let question = TerminalInput::Key(TerminalKey::character('?'));
        assert_eq!(parse_one(b"/"), slash);
        assert_eq!(parse_one(b"\x1b[47u"), slash);
        assert_eq!(parse_one(b"\x1b[47:63;2u"), question);
    }

    #[test]
    fn bracketed_paste_and_shifted_k_keep_editor_meaning() {
        assert_eq!(parse_one(b"\x1b[200~fn main() {}\x1b[201~"), TerminalInput::Paste("fn main() {}".to_owned()));
        let hover_key = TerminalInput::Key(TerminalKey::character('K'));
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
            b"\x1b[?1049h\x1b[?2004h\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?2027h\x1b[>5u\x1b_Ga=d,d=I,i=1465009486,q=2\x1b\\\x1b[m\x1b[?2026l\x1b[?25h\x1b[<1u\x1b[?2027l\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[?1049l"
        );
    }

    #[test]
    fn renderer_uses_typed_cursor_clear_and_visibility_sequences() {
        let mut output = Vec::new();
        render_patches(
            &mut output,
            &[TerminalPatch::Clear, TerminalPatch::MoveTo { column: 4, row: 2 }, TerminalPatch::ShowCursor(false), TerminalPatch::ShowCursor(true)],
            &mut None,
            &mut StyleCache::default(),
        )
        .expect("render");
        assert_eq!(output, b"\x1b[m\x1b[2J\x1b[3;5H\x1b[?25l\x1b[?25h");
    }

    #[test]
    fn renderer_transmits_and_deletes_kitty_rgb_overlays() {
        let overlay = std::sync::Arc::new(wren_view::RasterOverlay {
            frame_id: 1,
            width: 2,
            height: 1,
            columns: 1,
            rows: 1,
            source: RasterSource::Rgb(std::sync::Arc::new(vec![1_u8, 2, 3, 4, 5, 6])),
        });
        let mut output = Vec::new();
        let mut raster_rows = None;
        render_patches(&mut output, &[TerminalPatch::SetRasterOverlay(Some(overlay))], &mut raster_rows, &mut StyleCache::default()).expect("render overlay");
        assert_eq!(raster_rows, Some(1));
        assert_eq!(output, b"\x1b[1;1H\x1b_Ga=T,f=24,s=2,v=1,i=1465009486,p=1,c=1,r=1,C=1,z=-1,q=2,m=0;AQIDBAUG\x1b\\");

        output.clear();
        render_patches(&mut output, &[TerminalPatch::SetRasterOverlay(None)], &mut raster_rows, &mut StyleCache::default()).expect("delete overlay");
        assert_eq!(raster_rows, None);
        assert_eq!(output, b"\x1b_Ga=d,d=I,i=1465009486,q=2\x1b\\");
    }

    #[test]
    fn quad_rasterizer_clips_exact_geometry_to_image_bounds() {
        let quad = RasterQuad { vertices: [[-1.0, -1.0], [1.0, -1.0], [1.0, 3.0], [-1.0, 3.0]], color: wren_view::RgbColor::new(200, 210, 220) };
        let pixels = rasterize_quads(2, 2, wren_view::RgbColor::new(10, 20, 30), &[quad]).expect("rasterize");
        assert_eq!(pixels, vec![200, 210, 220, 10, 20, 30, 200, 210, 220, 10, 20, 30]);
    }

    #[test]
    fn kitty_vector_overlay_uses_compressed_high_resolution_pixels() {
        let overlay = std::sync::Arc::new(wren_view::RasterOverlay {
            frame_id: 2,
            width: 12,
            height: 12,
            columns: 2,
            rows: 1,
            source: RasterSource::Quads { background: wren_view::RgbColor::new(10, 20, 30), quads: std::sync::Arc::new(Vec::new()) },
        });
        let mut output = Vec::new();
        render_patches(&mut output, &[TerminalPatch::SetRasterOverlay(Some(overlay))], &mut None, &mut StyleCache::default()).expect("render vector overlay");
        assert!(output.windows(b"f=24,o=z,s=12,v=12".len()).any(|window| window == b"f=24,o=z,s=12,v=12"));
    }
}
