#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

pub use catppuccin::{ColorName as CatppuccinColor, FlavorName as CatppuccinFlavor};
use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use wren_engine::{EngineFrame, FrameText, FrameTextChange};
pub use wren_types::RgbColor;
use wren_types::{Bias, BufferId, DocumentRevision, Edit, TabId, Transaction, WindowId, ranges_overlap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowDirection {
    Left,
    Down,
    Up,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowView {
    pub id: WindowId,
    pub buffer_id: BufferId,
    pub top_line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitTree {
    Window(WindowView),
    Split { axis: SplitAxis, ratio_milli: u16, first: Box<SplitTree>, second: Box<SplitTree> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorHit {
    pub window_id: WindowId,
    pub buffer_id: BufferId,
    pub byte: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabView {
    pub id: TabId,
    pub root: SplitTree,
    pub active_window: WindowId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientViewModel {
    pub tabs: Vec<TabView>,
    pub active_tab: TabId,
    next_id: u64,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ViewModelError {
    #[error("unknown window {0:?}")]
    UnknownWindow(WindowId),
    #[error("cannot close the final window")]
    FinalWindow,
    #[error("cannot close the final tab")]
    FinalTab,
}

impl ClientViewModel {
    #[must_use]
    pub fn initial() -> Self {
        let buffer_id = BufferId::new(1);
        let window_id = WindowId::new(2);
        let tab_id = TabId::new(4);
        Self {
            tabs: vec![TabView { id: tab_id, root: SplitTree::Window(WindowView { id: window_id, buffer_id, top_line: 0 }), active_window: window_id }],
            active_tab: tab_id,
            next_id: 5,
        }
    }

    #[must_use]
    pub fn active_tab(&self) -> &TabView {
        &self.tabs[self.active_tab_index()]
    }

    #[must_use]
    pub fn active_window(&self) -> &WindowView {
        let tab = self.active_tab();
        find_window(&tab.root, tab.active_window).unwrap_or_else(|| first_window(&tab.root))
    }

    pub fn active_window_mut(&mut self) -> &mut WindowView {
        let index = self.active_tab_index();
        let id = self.tabs[index].active_window;
        selected_window_mut(&mut self.tabs[index].root, id)
    }

    #[must_use]
    pub fn active_buffer(&self) -> BufferId {
        self.active_window().buffer_id
    }

    pub fn add_buffer(&mut self) -> BufferId {
        BufferId::new(self.allocate())
    }

    pub fn set_active_buffer(&mut self, buffer_id: BufferId) {
        self.active_window_mut().buffer_id = buffer_id;
    }

    pub fn remove_buffer(&mut self, buffer_id: BufferId, replacement: BufferId) {
        for tab in &mut self.tabs {
            visit_windows_mut(&mut tab.root, &mut |window| {
                if window.buffer_id == buffer_id {
                    window.buffer_id = replacement;
                }
            });
        }
    }

    pub fn split_active(&mut self, axis: SplitAxis) -> Result<WindowId, ViewModelError> {
        let active = self.active_tab().active_window;
        let buffer_id = self.active_window().buffer_id;
        let window_id = WindowId::new(self.allocate());
        let window = WindowView { id: window_id, buffer_id, top_line: 0 };
        let tab_index = self.active_tab_index();
        let tab = &mut self.tabs[tab_index];
        replace_window_with_split(&mut tab.root, active, &window, axis)?;
        tab.active_window = window_id;
        Ok(window_id)
    }

    pub fn close_active_window(&mut self) -> Result<(), ViewModelError> {
        let active = self.active_tab().active_window;
        let tab_index = self.active_tab_index();
        let tab = &mut self.tabs[tab_index];
        let replacement = remove_window(&mut tab.root, active).ok_or(ViewModelError::FinalWindow)?;
        tab.active_window = replacement;
        Ok(())
    }

    pub fn new_tab(&mut self, buffer_id: BufferId) -> TabId {
        let window_id = WindowId::new(self.allocate());
        let tab_id = TabId::new(self.allocate());
        self.tabs.push(TabView { id: tab_id, root: SplitTree::Window(WindowView { id: window_id, buffer_id, top_line: 0 }), active_window: window_id });
        self.active_tab = tab_id;
        tab_id
    }

    pub fn cycle_tab(&mut self, delta: isize) {
        let current = self.active_tab_index();
        let len = self.tabs.len();
        let next = current.wrapping_add_signed(delta).rem_euclid(len);
        self.active_tab = self.tabs[next].id;
    }

    pub fn focus_window(&mut self, direction: WindowDirection) -> Result<(), ViewModelError> {
        let tab_index = self.active_tab_index();
        let active = self.tabs[tab_index].active_window;
        let mut panes = Vec::new();
        split_rectangles(&self.tabs[tab_index].root, Rect { column: 0, row: 0, width: 1_000_000, height: 1_000_000 }, &mut panes);
        let current = panes.iter().find(|(window, _)| *window == active).map(|(_, rectangle)| *rectangle).ok_or(ViewModelError::UnknownWindow(active))?;
        let horizontal = matches!(direction, WindowDirection::Left | WindowDirection::Right);
        let increasing = matches!(direction, WindowDirection::Right | WindowDirection::Down);
        let coordinates = |rectangle: Rect| {
            let x = rectangle.column.saturating_mul(2) + rectangle.width;
            let y = rectangle.row.saturating_mul(2) + rectangle.height;
            if horizontal { (x, y) } else { (y, x) }
        };
        let perpendicular_span = |rectangle: Rect| {
            if horizontal {
                rectangle.row..rectangle.row.saturating_add(rectangle.height)
            } else {
                rectangle.column..rectangle.column.saturating_add(rectangle.width)
            }
        };
        let (current_primary, current_perpendicular) = coordinates(current);
        let current_span = perpendicular_span(current);
        let candidate = panes
            .into_iter()
            .filter(|(window, _)| *window != active)
            .filter_map(|(window, rectangle)| {
                let (primary, perpendicular) = coordinates(rectangle);
                if increasing != (primary > current_primary) {
                    return None;
                }
                let overlaps = ranges_overlap(&current_span, &perpendicular_span(rectangle));
                Some((window, (!overlaps, primary.abs_diff(current_primary), perpendicular.abs_diff(current_perpendicular))))
            })
            .min_by_key(|(_, score)| *score)
            .map(|(window, _)| window);
        if let Some(candidate) = candidate {
            self.tabs[tab_index].active_window = candidate;
        }
        Ok(())
    }

    pub fn focus_window_id(&mut self, window_id: WindowId) -> Result<(), ViewModelError> {
        let tab_index = self.active_tab_index();
        let tab = &mut self.tabs[tab_index];
        if !has_window(&tab.root, window_id) {
            return Err(ViewModelError::UnknownWindow(window_id));
        }
        tab.active_window = window_id;
        Ok(())
    }

    pub fn cycle_window(&mut self, delta: isize) {
        let tab_index = self.active_tab_index();
        let ids = window_ids(&self.tabs[tab_index].root);
        let current = ids.iter().position(|id| *id == self.tabs[tab_index].active_window).unwrap_or_default();
        let next = current.wrapping_add_signed(delta).rem_euclid(ids.len());
        self.tabs[tab_index].active_window = ids[next];
    }

    pub fn only_active_window(&mut self) {
        let tab_index = self.active_tab_index();
        let window = self.active_window().clone();
        self.tabs[tab_index].root = SplitTree::Window(window);
    }

    pub fn equalize_windows(&mut self) {
        let tab_index = self.active_tab_index();
        equalize_split_tree(&mut self.tabs[tab_index].root);
    }

    pub fn close_active_tab(&mut self) -> Result<(), ViewModelError> {
        if self.tabs.len() == 1 {
            return Err(ViewModelError::FinalTab);
        }
        let current = self.active_tab_index();
        self.tabs.remove(current);
        self.active_tab = self.tabs[current.min(self.tabs.len() - 1)].id;
        Ok(())
    }

    #[must_use]
    pub fn window_count(&self) -> usize {
        self.tabs.iter().map(|tab| count_windows(&tab.root)).sum()
    }

    pub fn visit_windows(&self, mut visit: impl FnMut(&WindowView)) {
        for tab in &self.tabs {
            visit_windows(&tab.root, &mut visit);
        }
    }

    pub fn window_mut(&mut self, id: WindowId) -> Option<&mut WindowView> {
        self.tabs.iter_mut().find_map(|tab| find_window_mut(&mut tab.root, id))
    }

    fn allocate(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    fn active_tab_index(&self) -> usize {
        self.tabs.iter().position(|tab| tab.id == self.active_tab).unwrap_or_default()
    }
}

fn replace_window_with_split(tree: &mut SplitTree, target: WindowId, new: &WindowView, axis: SplitAxis) -> Result<(), ViewModelError> {
    match tree {
        SplitTree::Window(window) if window.id == target => {
            *tree = SplitTree::Split {
                axis,
                ratio_milli: 500,
                first: Box::new(SplitTree::Window(window.clone())),
                second: Box::new(SplitTree::Window(new.clone())),
            };
            Ok(())
        }
        SplitTree::Window(_) => Err(ViewModelError::UnknownWindow(target)),
        SplitTree::Split { first, second, .. } => {
            replace_window_with_split(first, target, new, axis).or_else(|_| replace_window_with_split(second, target, new, axis))
        }
    }
}

fn remove_window(tree: &mut SplitTree, target: WindowId) -> Option<WindowId> {
    let SplitTree::Split { first, second, .. } = tree else {
        return None;
    };
    if matches!(first.as_ref(), SplitTree::Window(window) if window.id == target) {
        let sibling = std::mem::replace(second, Box::new((**first).clone()));
        let next = first_window(&sibling).id;
        *tree = *sibling;
        return Some(next);
    }
    if matches!(second.as_ref(), SplitTree::Window(window) if window.id == target) {
        let sibling = std::mem::replace(first, Box::new((**second).clone()));
        let next = first_window(&sibling).id;
        *tree = *sibling;
        return Some(next);
    }
    remove_window(first, target).or_else(|| remove_window(second, target))
}

fn first_window(tree: &SplitTree) -> &WindowView {
    match tree {
        SplitTree::Window(window) => window,
        SplitTree::Split { first, .. } => first_window(first),
    }
}

fn find_window(tree: &SplitTree, id: WindowId) -> Option<&WindowView> {
    match tree {
        SplitTree::Window(window) => (window.id == id).then_some(window),
        SplitTree::Split { first, second, .. } => find_window(first, id).or_else(|| find_window(second, id)),
    }
}

fn find_window_mut(tree: &mut SplitTree, id: WindowId) -> Option<&mut WindowView> {
    match tree {
        SplitTree::Window(window) => (window.id == id).then_some(window),
        SplitTree::Split { first, second, .. } => find_window_mut(first, id).or_else(|| find_window_mut(second, id)),
    }
}

fn selected_window_mut(tree: &mut SplitTree, id: WindowId) -> &mut WindowView {
    match tree {
        SplitTree::Window(window) => window,
        SplitTree::Split { first, second, .. } if has_window(first, id) => selected_window_mut(first, id),
        SplitTree::Split { second, .. } => selected_window_mut(second, id),
    }
}

fn has_window(tree: &SplitTree, id: WindowId) -> bool {
    find_window(tree, id).is_some()
}

fn count_windows(tree: &SplitTree) -> usize {
    match tree {
        SplitTree::Window(_) => 1,
        SplitTree::Split { first, second, .. } => count_windows(first) + count_windows(second),
    }
}

fn visit_windows(tree: &SplitTree, visit: &mut impl FnMut(&WindowView)) {
    match tree {
        SplitTree::Window(window) => visit(window),
        SplitTree::Split { first, second, .. } => {
            visit_windows(first, visit);
            visit_windows(second, visit);
        }
    }
}

fn visit_windows_mut(tree: &mut SplitTree, visit: &mut impl FnMut(&mut WindowView)) {
    match tree {
        SplitTree::Window(window) => visit(window),
        SplitTree::Split { first, second, .. } => {
            visit_windows_mut(first, visit);
            visit_windows_mut(second, visit);
        }
    }
}

fn window_ids(tree: &SplitTree) -> Vec<WindowId> {
    match tree {
        SplitTree::Window(window) => vec![window.id],
        SplitTree::Split { first, second, .. } => {
            let mut ids = window_ids(first);
            ids.extend(window_ids(second));
            ids
        }
    }
}

fn equalize_split_tree(tree: &mut SplitTree) {
    if let SplitTree::Split { ratio_milli, first, second, .. } = tree {
        *ratio_milli = 500;
        equalize_split_tree(first);
        equalize_split_tree(second);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CellColor {
    Palette(u8),
    Rgb(RgbColor),
}

impl CellColor {
    #[must_use]
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self::Rgb(RgbColor::new(red, green, blue))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatppuccinPalette {
    slots: [RgbColor; 26],
}

impl CatppuccinPalette {
    #[must_use]
    pub fn for_flavor(flavor: CatppuccinFlavor) -> Self {
        let colors = &catppuccin::PALETTE[flavor].colors;
        let mut slots = [RgbColor::new(0, 0, 0); 26];
        for color in colors {
            slots[color.order as usize] = RgbColor::new(color.rgb.r, color.rgb.g, color.rgb.b);
        }
        Self { slots }
    }

    #[must_use]
    pub fn color(self, name: CatppuccinColor) -> RgbColor {
        self.slots[catppuccin::PALETTE.mocha.colors[name].order as usize]
    }

    pub fn set(&mut self, name: &str, color: RgbColor) -> bool {
        let Ok(name) = name.parse::<CatppuccinColor>() else { return false };
        self.slots[catppuccin::PALETTE.mocha.colors[name].order as usize] = color;
        true
    }

    fn remap_from_mocha(self, color: CellColor) -> CellColor {
        let CellColor::Rgb(rgb) = color else {
            return color;
        };
        Self::for_flavor(CatppuccinFlavor::Mocha)
            .slots
            .into_iter()
            .zip(self.slots)
            .find_map(|(source, target)| (source == rgb).then_some(CellColor::Rgb(target)))
            .unwrap_or(color)
    }
}

#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellStyle {
    pub attributes: u8,
    /// True color is preferred; palette indexes remain available for terminal-native colors.
    pub foreground: Option<CellColor>,
    pub background: Option<CellColor>,
}

impl Default for CellStyle {
    fn default() -> Self {
        Self {
            attributes: 0,
            foreground: Some(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Text))),
            background: Some(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Base))),
        }
    }
}

impl CellStyle {
    pub const fn rgb(foreground: RgbColor, background: RgbColor) -> Self {
        Self { attributes: 0, foreground: Some(CellColor::Rgb(foreground)), background: Some(CellColor::Rgb(background)) }
    }

    pub const fn with_foreground(mut self, color: CellColor) -> Self {
        self.foreground = Some(color);
        self
    }

    pub const fn with_background(mut self, color: CellColor) -> Self {
        self.background = Some(color);
        self
    }

    pub const fn without_foreground(mut self) -> Self {
        self.foreground = None;
        self
    }

    #[cfg(test)]
    pub const fn without_background(mut self) -> Self {
        self.background = None;
        self
    }

    pub const fn with_bold(mut self) -> Self {
        self.attributes |= 1;
        self
    }

    pub const fn with_underline(mut self) -> Self {
        self.attributes |= 4;
        self
    }

    pub const fn with_reverse(mut self) -> Self {
        self.attributes |= 16;
        self
    }

    pub const fn bold(self) -> bool {
        self.attributes & 1 != 0
    }
    pub const fn italic(self) -> bool {
        self.attributes & 2 != 0
    }
    pub const fn underline(self) -> bool {
        self.attributes & 4 != 0
    }
    pub const fn strikethrough(self) -> bool {
        self.attributes & 8 != 0
    }
    pub const fn reverse(self) -> bool {
        self.attributes & 16 != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuOverlayRow {
    pub label: Box<str>,
    pub detail: Box<str>,
    pub source: Option<Box<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PickerOverlay {
    pub title: Box<str>,
    pub prompt: Box<str>,
    pub rows: Vec<MenuOverlayRow>,
    pub selected: usize,
    pub preview_title: Box<str>,
    pub preview: Box<str>,
    pub preview_scroll: usize,
    pub preview_highlight_line: Option<usize>,
    pub preview_decorations: Vec<DecorationSpan>,
    pub footer: Box<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionOverlay {
    pub rows: Vec<MenuOverlayRow>,
    pub selected: Option<usize>,
    pub documentation: Box<str>,
    pub documentation_scroll: usize,
}

#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPopup {
    pub title: Box<str>,
    pub text: Box<str>,
    pub scroll: usize,
    /// A cursor is present only while keyboard focus is inside the popup.
    /// Coordinates address wrapped display rows and display columns.
    pub cursor: Option<(usize, usize)>,
    pub decorations: Vec<DecorationSpan>,
}

/// A terminal-native application docked at the right edge of the editor.
///
/// The cells have already been interpreted by the terminal emulator, so the
/// view must preserve their graphemes and styles instead of re-rendering their
/// contents as editor text.
#[derive(Debug, Clone, Copy)]
pub struct TerminalSidebar<'a> {
    pub rows: &'a [CellRow],
    pub cursor: (usize, usize),
    pub focused: bool,
}

impl TextPopup {
    pub fn new(title: impl Into<Box<str>>, text: impl Into<Box<str>>) -> Self {
        Self { title: title.into(), text: text.into(), scroll: 0, cursor: None, decorations: Vec::new() }
    }

    #[cfg(test)]
    pub fn with_cursor(mut self, cursor: (usize, usize)) -> Self {
        self.cursor = Some(cursor);
        self
    }

    pub fn with_decorations(mut self, decorations: Vec<DecorationSpan>) -> Self {
        self.decorations = decorations;
        self
    }

    /// Widths of the display rows used by popup rendering. Keeping navigation
    /// on this calculation prevents the focused cursor from drifting away from
    /// long lines after they wrap.
    #[must_use]
    pub fn navigation_line_widths(&self, terminal_width: usize, tab_width: usize) -> Vec<usize> {
        wrap_popup_text(&self.text, popup_max_inner_width(terminal_width), tab_width).iter().map(|line| display_width(&line.text, tab_width)).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AceJumpTarget {
    pub byte: usize,
    pub label: Box<str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AceJumpOverlay {
    pub targets: Vec<AceJumpTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugOverlay {
    pub panels: [DebugPanel; 6],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugPanel {
    pub title: &'static str,
    pub text: Box<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusSegment {
    pub text: Box<str>,
    pub style: CellStyle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusOverlay {
    pub left: Vec<StatusSegment>,
    pub right: Vec<StatusSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecorationSpan {
    pub range: Range<usize>,
    pub style: CellStyle,
    #[serde(default = "default_decoration_priority")]
    pub priority: u32,
}

impl DecorationSpan {
    #[must_use]
    pub const fn new(range: Range<usize>, style: CellStyle, priority: u32) -> Self {
        Self { range, style, priority }
    }
}

pub type SharedDecorations = Arc<Vec<DecorationSpan>>;

const fn default_decoration_priority() -> u32 {
    1_000_000
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineDecoration {
    pub line: usize,
    pub style: CellStyle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub grapheme: CellGrapheme,
    pub width: u8,
    pub style: CellStyle,
}

/// Retains ordinary graphemes inline while preserving arbitrary clusters.
pub type CellGrapheme = CompactString;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CellRow {
    pub cells: Vec<Cell>,
}

impl CellRow {
    #[must_use]
    #[cfg(test)]
    pub fn text(&self) -> String {
        self.cells.iter().map(|cell| cell.grapheme.as_str()).collect()
    }
}

#[must_use]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesiredGrid {
    pub epoch: u64,
    pub width: usize,
    pub height: usize,
    pub rows: Vec<Arc<CellRow>>,
    pub cursor: (usize, usize),
    #[serde(skip)]
    pub raster_overlay: Option<Arc<RasterOverlay>>,
}

impl DesiredGrid {
    #[must_use]
    #[cfg(test)]
    pub fn text(&self) -> String {
        self.rows.iter().map(|row| row.text()).collect::<Vec<_>>().join("\n")
    }
}

/// A raster image or vector source that a capable terminal paints over cells.
/// Text cells remain available as the portable fallback.
#[derive(Debug, Clone)]
pub struct RasterOverlay {
    pub frame_id: u64,
    pub width: usize,
    pub height: usize,
    pub columns: usize,
    pub rows: usize,
    pub background: RgbColor,
    pub quads: Arc<Vec<RasterQuad>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RasterQuad {
    pub vertices: [[f32; 2]; 4],
    pub color: RgbColor,
    pub border: Option<RasterBorder>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RasterBorder {
    pub color: RgbColor,
    pub width: f32,
}

impl PartialEq for RasterOverlay {
    fn eq(&self, other: &Self) -> bool {
        self.frame_id == other.frame_id && self.width == other.width && self.height == other.height && self.columns == other.columns && self.rows == other.rows
    }
}

impl Eq for RasterOverlay {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedEditorRender {
    text: FrameText,
    content_row_lines: Vec<Option<usize>>,
    top_line: usize,
    settings: EditorRenderSettings,
    status: Box<str>,
    prompt: Option<Box<str>>,
    decorations: SharedDecorations,
    line_decorations: Vec<LineDecoration>,
    relative_cursor_line: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EditorRenderSettings {
    width: usize,
    height: usize,
    tab_width: usize,
    line_numbers: bool,
    relative_numbers: bool,
    number_width: usize,
    color_column: Option<usize>,
    theme: CatppuccinPalette,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedLogicalRow {
    rows: Vec<Arc<CellRow>>,
    decorations: Vec<DecorationSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedEditorViewport {
    render: CachedEditorRender,
    rows: Vec<Arc<CellRow>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedDecorationComposition {
    buffer_id: BufferId,
    layers: Vec<SharedDecorations>,
    merged: SharedDecorations,
}

#[derive(Clone, Copy)]
struct EditorGridLayout {
    content_height: usize,
    gutter: usize,
}

struct RenderedEditorContent {
    rows: Vec<CellRow>,
    row_lines: Vec<Option<usize>>,
    cursor: (usize, usize),
}

#[derive(Clone, Copy)]
struct EditorRenderRequest<'a> {
    frame: &'a EngineFrame,
    status: &'a str,
    prompt: Option<&'a str>,
    decorations: &'a [DecorationSpan],
    line_decorations: &'a [LineDecoration],
    shared: Option<&'a SharedDecorations>,
}

impl CachedEditorRender {
    fn matches_context(&self, request: EditorRenderRequest<'_>, top_line: usize, settings: EditorRenderSettings, relative_cursor_line: Option<usize>) -> bool {
        self.top_line == top_line
            && self.settings == settings
            && self.status.as_ref() == request.status
            && self.prompt.as_deref() == request.prompt
            && self.line_decorations == request.line_decorations
            && self.relative_cursor_line == relative_cursor_line
    }

    fn matches_frame(&self, request: EditorRenderRequest<'_>, top_line: usize, settings: EditorRenderSettings, relative_cursor_line: Option<usize>) -> bool {
        self.text.same_snapshot(&request.frame.text)
            && self.decorations.as_ref() == request.decorations
            && self.matches_context(request, top_line, settings, relative_cursor_line)
    }
}

struct WorkspacePaneSource<'a> {
    window: &'a WindowView,
    frame: &'a EngineFrame,
    decorations: &'a SharedDecorations,
    line_decorations: &'a [LineDecoration],
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TerminalUpdate {
    pub clear: bool,
    pub rows: Vec<(usize, Arc<CellRow>)>,
    pub raster_overlay: Option<Option<Arc<RasterOverlay>>>,
    pub cursor: (usize, usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewportLayout {
    pub width: usize,
    pub height: usize,
    pub top_line: usize,
    pub tab_width: usize,
    scrolloff: usize,
    line_numbers: bool,
    relative_numbers: bool,
    number_width: usize,
    color_column: Option<usize>,
    theme: CatppuccinPalette,
    epoch: u64,
    cached_rows: Vec<Arc<CellRow>>,
    cached_logical_rows: BTreeMap<(usize, Option<usize>), CachedLogicalRow>,
    cached_editor_render: Option<CachedEditorRender>,
    cached_editor_viewports: Vec<CachedEditorViewport>,
    cached_decoration_compositions: Vec<CachedDecorationComposition>,
    cached_status_rows: Vec<(StatusOverlay, Arc<CellRow>)>,
    pane_layouts: BTreeMap<WindowId, Box<ViewportLayout>>,
    workspace_right_margin: usize,
    realtime_updates_prepared: bool,
}

impl ViewportLayout {
    #[must_use]
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            top_line: 0,
            tab_width: 4,
            scrolloff: 0,
            line_numbers: false,
            relative_numbers: false,
            number_width: 0,
            color_column: None,
            theme: CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha),
            epoch: 0,
            cached_rows: Vec::new(),
            cached_logical_rows: BTreeMap::new(),
            cached_editor_render: None,
            cached_editor_viewports: Vec::new(),
            cached_decoration_compositions: Vec::new(),
            cached_status_rows: Vec::new(),
            pane_layouts: BTreeMap::new(),
            workspace_right_margin: 0,
            realtime_updates_prepared: false,
        }
    }

    pub fn set_theme(&mut self, theme: CatppuccinPalette) {
        if self.theme != theme {
            self.theme = theme;
            self.clear_cached_rendering();
            self.epoch = self.epoch.saturating_add(1);
        }
    }

    pub fn configure_dotfile_profile(&mut self) {
        self.tab_width = 2;
        self.scrolloff = 3;
        self.line_numbers = true;
        self.relative_numbers = true;
        self.number_width = 3;
        self.color_column = Some(80);
        self.clear_cached_rendering();
        self.epoch = self.epoch.saturating_add(1);
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.epoch = self.epoch.saturating_add(1);
        self.clear_cached_rendering();
    }

    /// Reserves the right-hand dock before workspace panes are laid out. This
    /// makes the harness behave like an editor split: text wraps before the sidebar
    /// and no source cells are merely hidden underneath it.
    pub fn set_terminal_sidebar_visible(&mut self, visible: bool) {
        let margin = if visible { self.terminal_sidebar_column().map_or(0, |column| self.width.saturating_sub(column)) } else { 0 };
        if self.workspace_right_margin != margin {
            self.workspace_right_margin = margin;
            self.clear_cached_rendering();
            self.epoch = self.epoch.saturating_add(1);
        }
    }

    fn workspace_width(&self) -> usize {
        self.width.saturating_sub(self.workspace_right_margin).max(1)
    }

    fn clear_cached_rendering(&mut self) {
        self.cached_rows.clear();
        self.cached_logical_rows.clear();
        self.cached_editor_render = None;
        self.cached_editor_viewports.clear();
        self.cached_decoration_compositions.clear();
        self.cached_status_rows.clear();
        self.pane_layouts.clear();
    }

    /// Canonicalizes separately owned decoration layers without rebuilding an
    /// identical flattened span list on every cursor-only frame. Layer values
    /// are compared when their allocations differ, so cache reuse cannot make
    /// changed search, diagnostic, semantic, or selection styling stale.
    #[must_use]
    pub fn compose_shared_decoration_layers(&mut self, decorations: &[(BufferId, Vec<SharedDecorations>)]) -> Vec<(BufferId, SharedDecorations)> {
        decorations.iter().map(|(buffer_id, layers)| (*buffer_id, self.compose_shared_decoration_buffer(*buffer_id, layers))).collect()
    }

    fn compose_shared_decoration_buffer(&mut self, buffer_id: BufferId, layers: &[SharedDecorations]) -> SharedDecorations {
        if layers.len() == 1 {
            return Arc::clone(&layers[0]);
        }
        if let Some(index) = self
            .cached_decoration_compositions
            .iter()
            .position(|cached| cached.buffer_id == buffer_id && shared_decoration_layers_equal(&cached.layers, layers))
        {
            let cached = self.cached_decoration_compositions.swap_remove(index);
            let merged = Arc::clone(&cached.merged);
            self.cached_decoration_compositions.push(cached);
            return merged;
        }
        let merged = Arc::new(merge_shared_decoration_layers(layers));
        self.cached_decoration_compositions.push(CachedDecorationComposition {
            buffer_id,
            layers: layers.iter().map(Arc::clone).collect(),
            merged: Arc::clone(&merged),
        });
        const DECORATION_COMPOSITION_CACHE_CAPACITY: usize = 16;
        if self.cached_decoration_compositions.len() > DECORATION_COMPOSITION_CACHE_CAPACITY {
            self.cached_decoration_compositions.remove(0);
        }
        merged
    }

    /// Maps a terminal cell back through the same pane, gutter, tab, Unicode,
    /// and soft-wrap geometry used to render the workspace.
    pub fn hit_test_workspace(
        &self,
        model: &ClientViewModel,
        frames: &[(BufferId, EngineFrame)],
        column: usize,
        row: usize,
        reserved_rows: usize,
    ) -> Option<EditorHit> {
        let overlay_start = self.height.saturating_sub(reserved_rows.min(self.height));
        if reserved_rows > 0 && row >= overlay_start {
            return None;
        }
        let content_height = self.height.saturating_sub(reserved_rows).max(1);
        let workspace_width = self.workspace_width();
        if column >= workspace_width || row >= content_height {
            return None;
        }
        let mut panes = Vec::new();
        split_rectangles(&model.active_tab().root, Rect { column: 0, row: 0, width: workspace_width, height: content_height }, &mut panes);
        let (window_id, rectangle) = panes.into_iter().find(|(_, rectangle)| {
            column >= rectangle.column
                && column < rectangle.column.saturating_add(rectangle.width)
                && row >= rectangle.row
                && row < rectangle.row.saturating_add(rectangle.height)
        })?;
        let window = find_window(&model.active_tab().root, window_id)?;
        let (_, frame) = frames.iter().find(|(buffer_id, _)| *buffer_id == window.buffer_id)?;
        let mut pane = Self::new(rectangle.width.max(1), rectangle.height.max(1));
        pane.top_line = window.top_line;
        pane.tab_width = self.tab_width;
        pane.scrolloff = self.scrolloff;
        pane.line_numbers = self.line_numbers;
        pane.relative_numbers = self.relative_numbers;
        pane.number_width = self.number_width;
        pane.color_column = self.color_column;
        pane.theme = self.theme;
        pane.ensure_cursor_visible(frame, 0);
        let gutter = if pane.line_numbers { pane.number_width.min(pane.width.saturating_sub(1)) } else { 0 };
        let content_width = pane.width.saturating_sub(gutter).max(1);
        let local_column = column.saturating_sub(rectangle.column).saturating_sub(gutter);
        let local_row = row.saturating_sub(rectangle.row);
        let start_byte = frame.text.byte_of_line(pane.top_line);
        let byte = byte_at_visual_cell(&frame.text, start_byte, content_width, pane.tab_width, local_column, local_row)?;
        Some(EditorHit { window_id, buffer_id: window.buffer_id, byte })
    }

    pub fn ensure_cursor_visible(&mut self, frame: &EngineFrame, reserved_rows: usize) {
        let cursor = frame.cursor_byte.min(frame.text.len());
        let cursor_line = frame.text.line_of_byte(cursor);
        let content_height = self.height.saturating_sub(reserved_rows).max(1);
        let margin = self.scrolloff.min(content_height.saturating_sub(1).checked_div(2).unwrap_or(0));
        if cursor_line < self.top_line.saturating_add(margin) {
            self.top_line = cursor_line.saturating_sub(margin);
            self.epoch = self.epoch.saturating_add(1);
        } else if cursor_line.saturating_add(margin) >= self.top_line + content_height {
            self.top_line = cursor_line.saturating_add(margin).saturating_add(1).saturating_sub(content_height);
            self.epoch = self.epoch.saturating_add(1);
        }
    }

    #[cfg(any(test, feature = "benchmarking"))]
    pub fn desired_grid(&mut self, frame: &EngineFrame) -> DesiredGrid {
        self.desired_editor_grid_with_line_decorations(frame, "", None, &[], &[])
    }

    pub fn desired_editor_grid(&mut self, frame: &EngineFrame, status: &str, prompt: Option<&str>) -> DesiredGrid {
        self.desired_editor_grid_with_line_decorations(frame, status, prompt, &[], &[])
    }

    pub fn desired_editor_grid_with_line_decorations(
        &mut self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) -> DesiredGrid {
        self.desired_editor_grid_retaining_decorations(frame, status, prompt, decorations, line_decorations, None)
    }

    pub fn desired_editor_grid_with_shared_decorations(
        &mut self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &SharedDecorations,
        line_decorations: &[LineDecoration],
    ) -> DesiredGrid {
        self.desired_editor_grid_retaining_decorations(frame, status, prompt, decorations, line_decorations, Some(decorations))
    }

    fn desired_editor_grid_retaining_decorations(
        &mut self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
        shared: Option<&SharedDecorations>,
    ) -> DesiredGrid {
        let request = EditorRenderRequest { frame, status, prompt, decorations, line_decorations, shared };
        let cached = self.cached_rows.len() == self.height
            && self.cached_editor_render.as_ref().is_some_and(|cached| {
                cached.matches_frame(
                    request,
                    self.top_line,
                    self.editor_render_settings(),
                    self.relative_numbers.then(|| request.frame.text.line_of_byte(request.frame.cursor_byte)),
                )
            });
        if cached {
            return self.reuse_editor_render(frame, status, prompt);
        }
        if let Some(grid) = self.repaint_cached_editor_line(request) {
            return grid;
        }
        if self.reuse_recent_editor_viewport(request) {
            return self.reuse_editor_render(frame, status, prompt);
        }
        if let Some(grid) = self.reconcile_cached_editor(request) {
            return grid;
        }
        let preserve_logical_rows =
            self.cached_editor_render.as_ref().is_some_and(|cached| cached.text.same_snapshot(&frame.text) && self.editor_cache_is_compatible(request));
        if !preserve_logical_rows {
            self.cached_logical_rows.clear();
        }
        let layout = EditorGridLayout { content_height: self.editor_content_height(status, prompt), gutter: self.editor_gutter(self.width) };
        let mut content = self.render_editor_content(frame, decorations, line_decorations, layout);
        if let Some((row, prompt_cursor)) = self.editor_footer(status, prompt) {
            content.rows.resize_with(layout.content_height, CellRow::default);
            content.rows.push(row);
            if let Some(prompt_cursor) = prompt_cursor {
                content.cursor = prompt_cursor;
            }
        }
        let cursor = content.cursor;
        let grid = self.finish_rows(content.rows, cursor);
        self.cache_editor_render(request, content.row_lines, self.relative_numbers.then(|| request.frame.text.line_of_byte(request.frame.cursor_byte)));
        self.stash_current_viewport_rows();
        grid
    }

    fn repaint_cached_editor_line(&mut self, request: EditorRenderRequest<'_>) -> Option<DesiredGrid> {
        let cached = self.cached_editor_render.as_ref()?;
        let cursor_line = request.frame.text.line_of_byte(request.frame.cursor_byte);
        if cached.text.same_snapshot(&request.frame.text)
            || !cached.matches_context(request, self.top_line, self.editor_render_settings(), self.relative_numbers.then_some(cursor_line))
            || !self.editor_cache_is_compatible(request)
        {
            return None;
        }
        let change = single_line_change(&cached.text, &request.frame.text)?;
        let content_height = self.editor_content_height(request.status, request.prompt);
        let old_visible = cached.text.byte_of_line(self.top_line)..cached.text.byte_of_line(self.top_line.saturating_add(content_height));
        let new_visible = request.frame.text.byte_of_line(self.top_line)..request.frame.text.byte_of_line(self.top_line.saturating_add(content_height));
        let affected = decoration_change_range(&cached.decorations, request.decorations, change, old_visible, new_visible.clone());
        let start = affected.start.max(new_visible.start);
        let end = affected.end.min(new_visible.end);
        let mut repaints = Vec::new();
        if start < end {
            let first = request.frame.text.line_of_byte(start);
            let last = request.frame.text.line_of_byte(end.saturating_sub(1));
            if last.saturating_sub(first) >= 4 {
                return None;
            }
            for line in first..=last {
                let Some(screen_row) = cached.content_row_lines.iter().position(|row_line| *row_line == Some(line)) else {
                    continue;
                };
                let old_rows = cached.content_row_lines[screen_row..].iter().take_while(|row_line| **row_line == Some(line)).count();
                let line_range = logical_line_range(&request.frame.text, line);
                let rendered = self.render_editor_line(
                    &request.frame.text,
                    line,
                    cursor_line,
                    line_range.start,
                    line_range.end,
                    old_rows,
                    request.decorations,
                    request.line_decorations,
                );
                if rendered.len() != old_rows {
                    return None;
                }
                repaints.push((line, screen_row, rendered));
            }
        }
        for (line, screen_row, rendered) in repaints {
            self.remove_cached_logical_line(line);
            for (target, row) in self.cached_rows[screen_row..].iter_mut().zip(rendered) {
                *target = Arc::new(row);
            }
        }
        let cached = self.cached_editor_render.as_mut()?;
        cached.text.clone_from(&request.frame.text);
        cached.decorations = retain_decorations(request.decorations, request.shared);
        Some(self.reuse_editor_render(request.frame, request.status, request.prompt))
    }

    fn reuse_recent_editor_viewport(&mut self, request: EditorRenderRequest<'_>) -> bool {
        let cursor_line = request.frame.text.line_of_byte(request.frame.cursor_byte);
        let settings = self.editor_render_settings();
        let Some(index) = self.cached_editor_viewports.iter().position(|viewport| {
            viewport.rows.len() == self.height && viewport.render.matches_frame(request, self.top_line, settings, self.relative_numbers.then_some(cursor_line))
        }) else {
            return false;
        };
        let viewport = self.cached_editor_viewports.swap_remove(index);
        if let Some(render) = self.cached_editor_render.replace(viewport.render) {
            let rows = std::mem::replace(&mut self.cached_rows, viewport.rows);
            self.cached_editor_viewports.push(CachedEditorViewport { render, rows });
        } else {
            self.cached_rows = viewport.rows;
        }
        self.prune_recent_editor_viewports();
        true
    }

    fn prune_recent_editor_viewports(&mut self) {
        const CAPACITY: usize = 8;
        if self.cached_editor_viewports.len() > CAPACITY {
            self.cached_editor_viewports.drain(..self.cached_editor_viewports.len() - CAPACITY);
        }
    }

    fn editor_content_height(&self, status: &str, prompt: Option<&str>) -> usize {
        self.height.saturating_sub(usize::from(!status.is_empty() || prompt.is_some())).max(1)
    }

    fn editor_gutter(&self, width: usize) -> usize {
        if self.line_numbers { self.number_width.min(width.saturating_sub(1)) } else { 0 }
    }

    fn editor_render_settings(&self) -> EditorRenderSettings {
        EditorRenderSettings {
            width: self.width,
            height: self.height,
            tab_width: self.tab_width,
            line_numbers: self.line_numbers,
            relative_numbers: self.relative_numbers,
            number_width: self.number_width,
            color_column: self.color_column,
            theme: self.theme,
        }
    }

    fn render_editor_content(
        &self,
        frame: &EngineFrame,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
        layout: EditorGridLayout,
    ) -> RenderedEditorContent {
        let start_byte = frame.text.byte_of_line(self.top_line);
        let mut builder = GridBuilder::new(self.width.saturating_sub(layout.gutter), layout.content_height, self.tab_width, start_byte, self.top_line);
        let end_byte = frame.text.byte_of_line(self.top_line.saturating_add(layout.content_height));
        let visible = frame.text.slice(start_byte..end_byte);
        builder.push_grapheme_document(&visible, frame.cursor_byte, decorations);
        let mut row_lines = builder.row_lines.clone();
        row_lines.resize(layout.content_height, None);
        row_lines.truncate(layout.content_height);
        let mut rows = builder.rows;
        let mut cursor = builder.cursor.unwrap_or((0, 0));
        if layout.gutter > 0 {
            let cursor_line = frame.text.line_of_byte(frame.cursor_byte);
            prepend_line_numbers(&mut rows, &builder.row_lines, cursor_line, layout.gutter, self.relative_numbers, line_decorations);
            cursor.0 = cursor.0.saturating_add(layout.gutter);
        }
        if let Some(column) = self.color_column {
            mark_color_column(&mut rows, layout.gutter.saturating_add(column.saturating_sub(1)), self.width, self.theme.color(CatppuccinColor::Mantle));
        }
        RenderedEditorContent { rows, row_lines, cursor }
    }

    fn editor_footer(&self, status: &str, prompt: Option<&str>) -> Option<(CellRow, Option<(usize, usize)>)> {
        let label = prompt.or((!status.is_empty()).then_some(status))?;
        Some((row_from_text(label, self.width, prompt_style(prompt.is_some()), self.tab_width), prompt.map(|_| self.prompt_cursor(label))))
    }

    fn cache_editor_render(&mut self, request: EditorRenderRequest<'_>, content_row_lines: Vec<Option<usize>>, relative_cursor_line: Option<usize>) {
        self.cached_editor_render = Some(CachedEditorRender {
            text: request.frame.text.clone(),
            content_row_lines,
            top_line: self.top_line,
            settings: self.editor_render_settings(),
            status: request.status.into(),
            prompt: request.prompt.map(Into::into),
            decorations: retain_decorations(request.decorations, request.shared),
            line_decorations: request.line_decorations.to_vec(),
            relative_cursor_line,
        });
    }

    fn prompt_cursor(&self, label: &str) -> (usize, usize) {
        let input = label.split("  │  ").next().unwrap_or(label);
        (display_width(input, self.tab_width).min(self.width.saturating_sub(1)), self.height.saturating_sub(1))
    }

    fn editor_cache_is_compatible(&self, request: EditorRenderRequest<'_>) -> bool {
        let Some(cached) = &self.cached_editor_render else {
            return false;
        };
        self.cached_rows.len() == self.height && cached.settings == self.editor_render_settings() && cached.line_decorations == request.line_decorations
    }

    fn reconcile_cached_editor(&mut self, request: EditorRenderRequest<'_>) -> Option<DesiredGrid> {
        if !self.editor_cache_is_compatible(request) {
            return None;
        }
        let same_text = self.cached_editor_render.as_ref()?.text.same_snapshot(&request.frame.text);
        let changed_line = if same_text { None } else { Some(single_line_change(&self.cached_editor_render.as_ref()?.text, &request.frame.text)?) };
        let trust = self.logical_row_trust(request, changed_line);
        if let Some(changed_line) = changed_line {
            self.remove_cached_logical_line(changed_line.line);
        }

        let content_height = self.editor_content_height(request.status, request.prompt);
        let cursor_line = request.frame.text.line_of_byte(request.frame.cursor_byte);
        let last_line = request.frame.text.line_of_byte(request.frame.text.len());
        let mut decorations = DecorationResolver::new_at(request.decorations, request.frame.text.byte_of_line(self.top_line));
        let mut rows = Vec::with_capacity(self.height);
        let mut row_lines = Vec::with_capacity(content_height);
        let mut logical_line = self.top_line;
        while rows.len() < content_height && logical_line <= last_line {
            let remaining = content_height - rows.len();
            let trusted = trust.as_ref().is_some_and(|range| range.contains(&logical_line)) && self.has_cached_logical_line(logical_line);
            let line_range = logical_line_range(&request.frame.text, logical_line);
            let indices = decorations.overlapping_indices(line_range);
            let mut line_rows = self.cached_line_rows(request, logical_line, remaining, (!trusted).then_some(indices));
            row_lines.push(Some(logical_line));
            row_lines.resize(row_lines.len().saturating_add(line_rows.len().saturating_sub(1)), None);
            rows.append(&mut line_rows);
            logical_line += 1;
        }
        if rows.len() < content_height {
            for index in rows.len()..content_height {
                let cached_blank = self
                    .cached_editor_render
                    .as_ref()
                    .and_then(|cached| cached.content_row_lines.get(index))
                    .is_some_and(Option::is_none)
                    .then(|| self.cached_rows.get(index).cloned())
                    .flatten();
                rows.push(cached_blank.unwrap_or_else(|| {
                    let mut blank = CellRow::default();
                    ensure_row_backgrounds(std::slice::from_mut(&mut blank));
                    apply_theme_to_rows(std::slice::from_mut(&mut blank), self.theme);
                    Arc::new(blank)
                }));
            }
            row_lines.resize(content_height, None);
        }
        if let Some((mut footer, _)) = self.editor_footer(request.status, request.prompt) {
            ensure_row_backgrounds(std::slice::from_mut(&mut footer));
            apply_theme_to_rows(std::slice::from_mut(&mut footer), self.theme);
            rows.push(Arc::new(footer));
        }
        rows.resize_with(self.height, || Arc::new(CellRow::default()));
        rows.truncate(self.height);
        self.cached_rows = rows;
        self.cache_editor_render(request, row_lines, self.relative_numbers.then_some(cursor_line));
        self.prune_cached_logical_rows(content_height);
        Some(self.reuse_editor_render(request.frame, request.status, request.prompt))
    }

    fn logical_row_trust(&self, request: EditorRenderRequest<'_>, change: Option<FrameTextChange>) -> Option<Range<usize>> {
        let Some(cached) = &self.cached_editor_render else {
            return None;
        };
        if change.is_some() {
            return None;
        }
        if request.shared.is_some_and(|decorations| Arc::ptr_eq(decorations, &cached.decorations)) {
            return Some(0..usize::MAX);
        }
        let old_start = cached.content_row_lines.iter().flatten().copied().min().unwrap_or(cached.top_line);
        let old_end = cached.content_row_lines.iter().flatten().copied().max().unwrap_or(old_start).saturating_add(1);
        let new_end = self.top_line.saturating_add(self.editor_content_height(request.status, request.prompt));
        let start = old_start.max(self.top_line);
        let end = old_end.min(new_end);
        let bytes = request.frame.text.byte_of_line(start)..request.frame.text.byte_of_line(end);
        (start < end && decorations_match_range(&cached.decorations, request.decorations, &bytes)).then_some(start..end)
    }

    fn cached_line_rows(&mut self, request: EditorRenderRequest<'_>, line: usize, remaining: usize, decoration_indices: Option<&[usize]>) -> Vec<Arc<CellRow>> {
        let Range { start, end } = logical_line_range(&request.frame.text, line);
        let content_width = self.width.saturating_sub(self.editor_gutter(self.width)).max(1);
        let visible = request.frame.text.slice(start..end);
        let row_count = visual_line_rows_bounded(&visible, content_width, self.tab_width, remaining);
        let cursor_line = request.frame.text.line_of_byte(request.frame.cursor_byte);
        let key = (line, self.relative_numbers.then_some(cursor_line));
        let exact = self.cached_logical_rows.get(&key);
        let cached = exact.or_else(|| self.cached_logical_row(line)).filter(|cached| {
            decoration_indices.is_none_or(|indices| relative_decorations_equal(request.decorations, indices, start..end, &cached.decorations))
        });
        let (mut rows, stored_decorations, exact) = cached.map_or_else(
            || {
                let indices = decoration_indices.unwrap_or_default();
                let absolute = indices.iter().map(|index| request.decorations[*index].clone()).collect::<Vec<_>>();
                let relative = relative_decorations_from_indices(request.decorations, indices, start..end);
                let rows = self
                    .render_editor_line(&request.frame.text, line, cursor_line, start, end, row_count.min(remaining), &absolute, request.line_decorations)
                    .into_iter()
                    .map(Arc::new)
                    .collect();
                (rows, relative, false)
            },
            |cached| {
                let decorations = if exact.is_some() { Vec::new() } else { cached.decorations.clone() };
                (cached.rows.clone(), decorations, exact.is_some())
            },
        );
        if self.relative_numbers && !exact {
            for row in &mut rows {
                replace_line_number(Arc::make_mut(row), line, cursor_line, self.number_width, true, request.line_decorations);
            }
        }
        if row_count <= remaining && !exact {
            self.cached_logical_rows.insert(key, CachedLogicalRow { rows: rows.clone(), decorations: stored_decorations });
        }
        rows.truncate(remaining);
        rows
    }

    fn stash_current_viewport_rows(&mut self) {
        let Some(cached) = &self.cached_editor_render else {
            return;
        };
        let mut decorations = DecorationResolver::new_at(&cached.decorations, cached.text.byte_of_line(cached.top_line));
        let mut index = 0;
        while let Some(Some(line)) = cached.content_row_lines.get(index) {
            let end = cached.content_row_lines[index + 1..].iter().position(Option::is_some).map_or(cached.content_row_lines.len(), |next| index + next + 1);
            let Range { start, end: line_end } = logical_line_range(&cached.text, *line);
            let range = start..line_end;
            let indices = decorations.overlapping_indices(range.clone());
            let width = self.width.saturating_sub(self.editor_gutter(self.width)).max(1);
            if visual_line_rows_bounded(&cached.text.slice(start..line_end), width, self.tab_width, end - index) <= end - index {
                self.cached_logical_rows.insert(
                    (*line, cached.relative_cursor_line),
                    CachedLogicalRow {
                        rows: self.cached_rows[index..end].to_vec(),
                        decorations: relative_decorations_from_indices(&cached.decorations, indices, range),
                    },
                );
            }
            index = end;
        }
    }

    fn prune_cached_logical_rows(&mut self, content_height: usize) {
        let capacity = content_height.saturating_mul(8).max(content_height);
        while self.cached_logical_rows.len() > capacity {
            let Some(farthest) = self.cached_logical_rows.keys().max_by_key(|(line, _)| line.abs_diff(self.top_line)).copied() else {
                break;
            };
            self.cached_logical_rows.remove(&farthest);
        }
    }

    fn cached_logical_row(&self, line: usize) -> Option<&CachedLogicalRow> {
        self.cached_logical_rows.range((line, None)..=(line, Some(usize::MAX))).next().map(|(_, row)| row)
    }

    fn has_cached_logical_line(&self, line: usize) -> bool {
        self.cached_logical_row(line).is_some()
    }

    fn remove_cached_logical_line(&mut self, line: usize) {
        while let Some(key) = self.cached_logical_rows.range((line, None)..=(line, Some(usize::MAX))).next().map(|(key, _)| *key) {
            self.cached_logical_rows.remove(&key);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_editor_line(
        &self,
        text: &FrameText,
        logical_line: usize,
        cursor_line: usize,
        start: usize,
        end: usize,
        height: usize,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) -> Vec<CellRow> {
        let gutter = self.editor_gutter(self.width);
        let mut builder = GridBuilder::new(self.width.saturating_sub(gutter), height.max(1), self.tab_width, start, logical_line);
        let visible = text.slice(start..end);
        builder.push_grapheme_document(&visible, usize::MAX, decorations);
        let mut rows = builder.rows;
        if gutter > 0 {
            prepend_line_numbers(&mut rows, &builder.row_lines, cursor_line, gutter, self.relative_numbers, line_decorations);
        }
        if let Some(column) = self.color_column {
            mark_color_column(&mut rows, gutter.saturating_add(column.saturating_sub(1)), self.width, self.theme.color(CatppuccinColor::Mantle));
        }
        ensure_row_backgrounds(&mut rows);
        apply_theme_to_rows(&mut rows, self.theme);
        rows
    }

    fn reuse_editor_render(&mut self, frame: &EngineFrame, status: &str, prompt: Option<&str>) -> DesiredGrid {
        let content_height = self.editor_content_height(status, prompt);
        let gutter = self.editor_gutter(self.width);
        let cursor_byte = frame.cursor_byte.min(frame.text.len());
        let cursor_line = frame.text.line_of_byte(cursor_byte);
        let line_start = frame.text.byte_of_line(cursor_line);
        let visible = frame.text.slice(line_start..cursor_byte.max(line_start));
        let (column, wrapped_row) =
            cursor_visual_position(&visible, 0, visible.len(), self.width.saturating_sub(gutter).max(1), content_height, self.tab_width);
        let base_row =
            self.cached_editor_render.as_ref().and_then(|cached| cached.content_row_lines.iter().position(|line| *line == Some(cursor_line))).unwrap_or(0);
        let mut cursor = (column, base_row.saturating_add(wrapped_row).min(content_height.saturating_sub(1)));
        cursor.0 = cursor.0.saturating_add(gutter);
        if let Some(label) = prompt {
            cursor = self.prompt_cursor(label);
        }
        self.epoch = self.epoch.saturating_add(1);
        DesiredGrid { epoch: self.epoch, width: self.width, height: self.height, rows: self.cached_rows.clone(), cursor, raster_overlay: None }
    }

    #[cfg(any(test, feature = "benchmarking"))]
    pub fn desired_workspace_grid(&mut self, model: &ClientViewModel, frames: &[(BufferId, EngineFrame)], status: &str, prompt: Option<&str>) -> DesiredGrid {
        self.desired_workspace_grid_with_shared_decorations(model, frames, &[], &[], status, prompt)
    }

    #[cfg(test)]
    pub fn desired_workspace_grid_with_decorations(
        &mut self,
        model: &ClientViewModel,
        frames: &[(BufferId, EngineFrame)],
        decorations: &[(BufferId, Vec<DecorationSpan>)],
        status: &str,
        prompt: Option<&str>,
    ) -> DesiredGrid {
        let decorations = decorations.iter().map(|(buffer_id, spans)| (*buffer_id, Arc::new(spans.clone()))).collect::<Vec<_>>();
        self.desired_workspace_grid_with_shared_decorations(model, frames, &decorations, &[], status, prompt)
    }

    pub fn desired_workspace_grid_with_shared_decorations(
        &mut self,
        model: &ClientViewModel,
        frames: &[(BufferId, EngineFrame)],
        decorations: &[(BufferId, SharedDecorations)],
        line_decorations: &[(BufferId, Vec<LineDecoration>)],
        status: &str,
        prompt: Option<&str>,
    ) -> DesiredGrid {
        let content_height = self.editor_content_height(status, prompt);
        let tab = model.active_tab();
        let mut panes = Vec::new();
        split_rectangles(&tab.root, Rect { column: 0, row: 0, width: self.workspace_width(), height: content_height }, &mut panes);
        self.pane_layouts.retain(|window_id, _| panes.iter().any(|(visible, _)| visible == window_id));
        if let [(window_id, rectangle)] = panes.as_slice()
            && *rectangle == (Rect { column: 0, row: 0, width: self.workspace_width(), height: content_height })
            && let Some(source) = workspace_pane_source(model, frames, decorations, line_decorations, *window_id)
        {
            let grid = self.render_workspace_pane(*window_id, *rectangle, source.window.top_line, source.frame, source.decorations, source.line_decorations);
            let mut rows = grid.rows;
            let mut cursor = grid.cursor;
            if let Some((mut row, prompt_cursor)) = self.editor_footer(status, prompt) {
                ensure_row_backgrounds(std::slice::from_mut(&mut row));
                apply_theme_to_rows(std::slice::from_mut(&mut row), self.theme);
                rows.push(self.cached_rows.last().filter(|cached| cached.as_ref() == &row).cloned().unwrap_or_else(|| Arc::new(row)));
                if let Some(prompt_cursor) = prompt_cursor {
                    cursor = prompt_cursor;
                }
            }
            return self.finish_shared_rows(rows, cursor);
        }

        let mut rows = vec![CellRow::default(); self.height];
        let mut cursor = (0, 0);
        for &(window_id, rectangle) in &panes {
            let Some(source) = workspace_pane_source(model, frames, decorations, line_decorations, window_id) else {
                continue;
            };
            let grid = self.render_workspace_pane(window_id, rectangle, source.window.top_line, source.frame, source.decorations, source.line_decorations);
            merge_pane_rows(&mut rows, rectangle, &grid.rows);
            if window_id == tab.active_window {
                cursor = (rectangle.column + grid.cursor.0, rectangle.row + grid.cursor.1);
            }
        }
        draw_split_borders(&tab.root, 0, 0, self.workspace_width(), content_height, &mut rows);
        if let Some((row, prompt_cursor)) = self.editor_footer(status, prompt) {
            rows[self.height.saturating_sub(1)] = row;
            if let Some(prompt_cursor) = prompt_cursor {
                cursor = prompt_cursor;
            }
        }
        self.finish_rows(rows, cursor)
    }

    /// Prepare a likely distant navigation target without replacing the
    /// currently displayed workspace. Large-file clients use this for the
    /// document edges so `gg`/`G` can reuse fully composed rows.
    pub fn prefetch_workspace_viewport(
        &mut self,
        window_id: WindowId,
        frame: &EngineFrame,
        top_line: usize,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) {
        let Some(pane) = self.pane_layouts.get_mut(&window_id) else {
            return;
        };
        pane.prefetch_editor_viewport(frame, top_line, decorations, line_decorations);
    }

    /// Exercise incremental edit and viewport reuse against disposable layout
    /// clones. This removes one-time executable and allocator faults from the
    /// first real keystroke without changing the retained frame or document.
    pub fn prepare_workspace_realtime_updates(
        &mut self,
        window_id: WindowId,
        frame: &EngineFrame,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) {
        let Some(pane) = self.pane_layouts.get_mut(&window_id) else {
            return;
        };
        pane.prepare_editor_realtime_updates(frame, decorations, line_decorations);
    }

    fn prepare_editor_realtime_updates(&mut self, frame: &EngineFrame, decorations: &[DecorationSpan], line_decorations: &[LineDecoration]) {
        if self.realtime_updates_prepared || self.cached_editor_render.is_none() {
            return;
        }
        self.realtime_updates_prepared = true;

        let byte = frame.cursor_byte.min(frame.text.len());
        if let Ok(transaction) = Transaction::new(DocumentRevision::new(0), vec![Edit::new(byte..byte, "x")])
            && let Ok(text) = frame.text.edited(&transaction)
        {
            let mapped = decorations
                .iter()
                .filter_map(|span| {
                    let start = transaction.map_offset(span.range.start, Bias::Left).ok()?;
                    let end = transaction.map_offset(span.range.end, Bias::Right).ok()?;
                    (start < end).then_some(DecorationSpan::new(start..end, span.style, span.priority))
                })
                .collect::<Vec<_>>();
            let mut preview = self.clone();
            let grid = preview.desired_editor_grid_with_line_decorations(&EngineFrame::new(text, byte.saturating_add(1)), "", None, &mapped, line_decorations);
            let _ = std::hint::black_box(grid);
        }

        let amount = self.height.saturating_sub(1).checked_div(2).unwrap_or(1);
        let target_line = frame.text.line_of_byte(frame.cursor_byte).saturating_add(amount);
        if target_line <= frame.text.line_of_byte(frame.text.len()) {
            let mut preview = self.clone();
            preview.top_line = preview.top_line.saturating_add(amount);
            let moved = EngineFrame::new(frame.text.clone(), frame.text.byte_of_line(target_line));
            let grid = preview.desired_editor_grid_with_line_decorations(&moved, "", None, decorations, line_decorations);
            let _ = std::hint::black_box(grid);
            preview.top_line = self.top_line;
            let grid = preview.desired_editor_grid_with_line_decorations(frame, "", None, decorations, line_decorations);
            let _ = std::hint::black_box(grid);
        }
    }

    fn prefetch_editor_viewport(&mut self, frame: &EngineFrame, top_line: usize, decorations: &[DecorationSpan], line_decorations: &[LineDecoration]) {
        let already_cached = self.cached_editor_render.as_ref().is_some_and(|cached| cached.top_line == top_line && cached.text.same_snapshot(&frame.text))
            || self.cached_editor_viewports.iter().any(|viewport| viewport.render.top_line == top_line && viewport.render.text.same_snapshot(&frame.text));
        if already_cached {
            return;
        }
        let mut preview = Self::new(self.width, self.height);
        preview.top_line = top_line;
        preview.tab_width = self.tab_width;
        preview.scrolloff = self.scrolloff;
        preview.line_numbers = self.line_numbers;
        preview.relative_numbers = self.relative_numbers;
        preview.number_width = self.number_width;
        preview.color_column = self.color_column;
        preview.theme = self.theme;
        preview.ensure_cursor_visible(frame, 0);
        let _ = preview.desired_editor_grid_with_line_decorations(frame, "", None, decorations, line_decorations);
        preview.prepare_editor_realtime_updates(frame, decorations, line_decorations);
        if let Some(render) = preview.cached_editor_render {
            self.cached_editor_viewports.push(CachedEditorViewport { render, rows: preview.cached_rows });
            self.prune_recent_editor_viewports();
        }
    }

    fn finish_rows(&mut self, mut rows: Vec<CellRow>, cursor: (usize, usize)) -> DesiredGrid {
        rows.resize_with(self.height, CellRow::default);
        rows.truncate(self.height);
        ensure_row_backgrounds(&mut rows);
        apply_theme_to_rows(&mut rows, self.theme);
        let rows: Vec<_> = rows
            .into_iter()
            .enumerate()
            .map(|(index, row)| self.cached_rows.get(index).filter(|cached| cached.as_ref() == &row).cloned().unwrap_or_else(|| Arc::new(row)))
            .collect();
        self.finish_shared_rows(rows, cursor)
    }

    fn finish_shared_rows(&mut self, mut rows: Vec<Arc<CellRow>>, cursor: (usize, usize)) -> DesiredGrid {
        rows.resize_with(self.height, || Arc::new(CellRow::default()));
        rows.truncate(self.height);
        self.cached_rows.clone_from(&rows);
        self.epoch = self.epoch.saturating_add(1);
        DesiredGrid { epoch: self.epoch, width: self.width, height: self.height, rows, cursor, raster_overlay: None }
    }

    fn render_workspace_pane(
        &mut self,
        window_id: WindowId,
        rectangle: Rect,
        top_line: usize,
        frame: &EngineFrame,
        decorations: &SharedDecorations,
        line_decorations: &[LineDecoration],
    ) -> DesiredGrid {
        let pane = self.pane_layouts.entry(window_id).or_insert_with(|| Box::new(Self::new(rectangle.width.max(1), rectangle.height.max(1))));
        let pane_width = rectangle.width.max(1);
        let pane_height = rectangle.height.max(1);
        if pane.width != pane_width || pane.height != pane_height {
            pane.resize(pane_width, pane_height);
        }
        pane.top_line = top_line;
        pane.tab_width = self.tab_width;
        pane.scrolloff = self.scrolloff;
        pane.line_numbers = self.line_numbers;
        pane.relative_numbers = self.relative_numbers;
        pane.number_width = self.number_width;
        pane.color_column = self.color_column;
        pane.set_theme(self.theme);
        pane.ensure_cursor_visible(frame, 0);
        pane.desired_editor_grid_with_shared_decorations(frame, "", None, decorations, line_decorations)
    }

    /// Draws Telescope's horizontal results/preview layout as a floating surface.
    /// The overlay deliberately lives above the editor grid so picker interaction
    /// never mutates buffer or split state.
    pub fn apply_picker_overlay(&mut self, grid: DesiredGrid, picker: &PickerOverlay) -> DesiredGrid {
        if self.width < 20 || self.height < 8 {
            return grid;
        }
        let mut rows = overlay_rows(&grid);
        let geometry = self.picker_geometry();
        let base = CellStyle::rgb(self.theme.color(CatppuccinColor::Text), self.theme.color(CatppuccinColor::Mantle));
        let border = CellStyle::rgb(self.theme.color(CatppuccinColor::Blue), self.theme.color(CatppuccinColor::Mantle)).with_bold();
        let title = CellStyle::rgb(self.theme.color(CatppuccinColor::Lavender), self.theme.color(CatppuccinColor::Mantle)).with_bold();
        let selected_style = CellStyle::rgb(self.theme.color(CatppuccinColor::Text), self.theme.color(CatppuccinColor::Surface2)).with_bold();
        draw_popup_frame(&mut rows, geometry.rectangle(), base, border, "");
        let heading = format!(" {} ({}) ", picker.title, picker.rows.len());
        paint_text(&mut rows[geometry.row], geometry.column + 2, heading.len().min(geometry.width.saturating_sub(4)), &heading, title);
        paint_text(
            &mut rows[geometry.prompt_row],
            geometry.column + 1,
            geometry.inner_width,
            &format!("❯ {}", picker.prompt),
            CellStyle::rgb(self.theme.color(CatppuccinColor::Text), self.theme.color(CatppuccinColor::Surface0)),
        );
        let footer_width = display_width(&picker.footer, 1).min(geometry.result_width.saturating_sub(2));
        if footer_width > 0 {
            let detail = CellStyle::rgb(self.theme.color(CatppuccinColor::Subtext0), self.theme.color(CatppuccinColor::Mantle));
            paint_text(&mut rows[geometry.row + 1], geometry.column + 2, footer_width, &picker.footer, detail);
        }
        let selected = picker.selected.min(picker.rows.len().saturating_sub(1));
        let result_start =
            selected.saturating_sub(geometry.content_height.saturating_sub(1) / 2).min(picker.rows.len().saturating_sub(geometry.content_height));
        for (screen_index, item) in picker.rows.iter().skip(result_start).take(geometry.content_height).enumerate() {
            let item_index = result_start + screen_index;
            let marker = if item_index == selected { "❯ " } else { "  " };
            let label = if item.detail.is_empty() { format!("{marker}{}", item.label) } else { format!("{marker}{}  {}", item.label, item.detail) };
            paint_text(
                &mut rows[geometry.content_start + screen_index],
                geometry.column + 1,
                geometry.result_width,
                &label,
                if item_index == selected { selected_style } else { base },
            );
        }
        if geometry.preview_visible {
            let divider_column = geometry.column + 1 + geometry.result_width;
            for target in rows.iter_mut().take(geometry.prompt_row).skip(geometry.row + 1) {
                paint_text(target, divider_column, 1, "│", border);
            }
            let preview_column = divider_column + 1;
            paint_text(&mut rows[geometry.row + 1], preview_column, geometry.preview_width, &format!(" {} ", picker.preview_title), title);
            let preview_lines = wrap_popup_text(&picker.preview, geometry.preview_width, self.tab_width);
            for (line_offset, line) in preview_lines.iter().filter(|line| line.source_line >= picker.preview_scroll).take(geometry.content_height).enumerate() {
                let style = if picker.preview_highlight_line == Some(line.source_line) { selected_style } else { base };
                paint_text(&mut rows[geometry.content_start + line_offset], preview_column, geometry.preview_width, &line.text, style);
                paint_decorated_popup_text(
                    &mut rows[geometry.content_start + line_offset],
                    preview_column,
                    line,
                    &picker.preview_decorations,
                    style,
                    self.tab_width,
                );
            }
        }
        self.finish_overlay(
            grid,
            rows,
            (
                geometry.column.saturating_add(3).saturating_add(display_width(&picker.prompt, self.tab_width)).min(geometry.column + geometry.width - 2),
                geometry.prompt_row,
            ),
        )
    }

    fn picker_geometry(&self) -> PickerGeometry {
        let width = self.width.saturating_sub(4).clamp(16, 120);
        let height = self.height.saturating_sub(2).min(self.height.saturating_mul(4) / 5).max(8);
        let column = self.width.saturating_sub(width) / 2;
        let row = self.height.saturating_sub(height) / 2;
        let preview_visible = width >= 64;
        let inner_width = width.saturating_sub(2);
        let result_width = if preview_visible { inner_width.saturating_mul(2) / 5 } else { inner_width };
        let prompt_row = row + height - 2;
        let content_start = row + 2;
        PickerGeometry {
            width,
            height,
            column,
            row,
            preview_visible,
            inner_width,
            result_width,
            preview_width: inner_width.saturating_sub(result_width + usize::from(preview_visible)),
            prompt_row,
            content_start,
            content_height: prompt_row.saturating_sub(content_start),
        }
    }

    /// Draws an nvim-cmp compatible popup menu at the active cursor and a
    /// documentation window alongside it when the selected item has docs.
    pub fn apply_completion_overlay(&mut self, grid: DesiredGrid, completion: &CompletionOverlay) -> DesiredGrid {
        if completion.rows.is_empty() || self.width < 12 || self.height < 4 {
            return grid;
        }
        let cursor = grid.cursor;
        let mut rows = overlay_rows(&grid);
        let visible_rows = completion.rows.len().min(10);
        let menu_height = visible_rows + 2;
        let longest = completion
            .rows
            .iter()
            .map(|row| {
                display_width(&row.label, 1)
                    .saturating_add(display_width(&row.detail, 1))
                    .saturating_add(display_width(row.source.as_deref().unwrap_or_default(), 1))
                    .saturating_add(7)
            })
            .max()
            .unwrap_or(20);
        let menu_width = longest.clamp(20, 52).min(self.width.saturating_sub(1));
        let menu_column = cursor.0.min(self.width.saturating_sub(menu_width));
        let menu_row = if cursor.1 + menu_height + 1 < self.height { cursor.1 + 1 } else { cursor.1.saturating_sub(menu_height) };
        let selected = completion.selected.unwrap_or(0).min(completion.rows.len().saturating_sub(1));
        let start = selected.saturating_sub(visible_rows.saturating_sub(1) / 2).min(completion.rows.len().saturating_sub(visible_rows));
        let menu = Rect { column: menu_column, row: menu_row, width: menu_width, height: menu_height };
        let base = CellStyle::rgb(self.theme.color(CatppuccinColor::Text), self.theme.color(CatppuccinColor::Surface0));
        let border = CellStyle::rgb(self.theme.color(CatppuccinColor::Blue), self.theme.color(CatppuccinColor::Base));
        let selected_style = CellStyle::rgb(self.theme.color(CatppuccinColor::Text), self.theme.color(CatppuccinColor::Surface2)).with_bold();
        draw_popup_frame(&mut rows, menu, base, border, "completion");
        for (offset, item) in completion.rows.iter().skip(start).take(visible_rows).enumerate() {
            let item_index = start + offset;
            let is_selected = completion.selected == Some(item_index);
            paint_text(
                &mut rows[menu.row + 1 + offset],
                menu.column + 1,
                menu.width.saturating_sub(2),
                &format!(
                    "{} {}{} [{}]",
                    if is_selected { "❯" } else { " " },
                    item.label,
                    if item.detail.is_empty() { String::new() } else { format!(" · {}", item.detail) },
                    item.source.as_deref().unwrap_or_default()
                ),
                if is_selected { selected_style } else { base },
            );
        }
        if !completion.documentation.is_empty() && self.width >= 48 {
            let available_right = self.width.saturating_sub(menu.column + menu.width + 1);
            let docs_width = 50.min(self.width.saturating_sub(2)).min(if available_right >= 24 { available_right } else { menu.column.saturating_sub(1) });
            if docs_width >= 20 {
                let docs_column = if available_right >= docs_width { menu.column + menu.width + 1 } else { menu.column.saturating_sub(docs_width + 1) };
                let docs_height = (completion.documentation.lines().skip(completion.documentation_scroll).count().min(12) + 2)
                    .clamp(4, self.height.saturating_sub(menu.row).max(4));
                let docs_style = base.with_background(CellColor::Rgb(self.theme.color(CatppuccinColor::Mantle)));
                draw_popup_frame(
                    &mut rows,
                    Rect { column: docs_column, row: menu.row, width: docs_width, height: docs_height },
                    docs_style,
                    border,
                    "documentation",
                );
                for (offset, line) in completion.documentation.lines().skip(completion.documentation_scroll).take(docs_height.saturating_sub(2)).enumerate() {
                    paint_text(&mut rows[menu.row + 1 + offset], docs_column + 1, docs_width.saturating_sub(2), line, docs_style);
                }
            }
        }
        self.finish_overlay(grid, rows, cursor)
    }

    /// Rounded context float used by LSP hover/signature help, diagnostics,
    /// Git previews and other `open_floating_preview` surfaces.
    pub fn apply_text_popup(&mut self, grid: DesiredGrid, popup: &TextPopup) -> DesiredGrid {
        if popup.text.is_empty() || self.width < 8 || self.height < 4 {
            return grid;
        }
        let cursor = grid.cursor;
        let max_inner_width = popup_max_inner_width(self.width);
        let lines = wrap_popup_text(&popup.text, max_inner_width, self.tab_width);
        let inner_width = lines.iter().map(|line| display_width(&line.text, self.tab_width)).max().unwrap_or(1).clamp(1, max_inner_width);
        let width = inner_width + 2;
        let visible_capacity = lines.len().min(self.height.saturating_sub(3).max(1));
        let maximum_scroll = lines.len().saturating_sub(visible_capacity);
        let mut scroll = popup.scroll.min(maximum_scroll);
        let popup_cursor = popup.cursor.map(|(row, column)| {
            let row = row.min(lines.len().saturating_sub(1));
            if row < scroll {
                scroll = row;
            } else if row >= scroll.saturating_add(visible_capacity) {
                scroll = row.saturating_add(1).saturating_sub(visible_capacity);
            }
            let line_width = display_width(&lines[row].text, self.tab_width);
            (row, column.min(line_width.saturating_sub(1)))
        });
        let visible_lines = lines.len().saturating_sub(scroll).min(visible_capacity);
        let height = visible_lines + 2;
        let placement = text_popup_placement(&grid, width, height);
        let column = placement.column;
        let row = placement.row;
        let mut rows = overlay_rows(&grid);
        let base_style = CellStyle::rgb(self.theme.color(CatppuccinColor::Text), self.theme.color(CatppuccinColor::Mantle));
        let border_style = CellStyle::rgb(self.theme.color(CatppuccinColor::Blue), self.theme.color(CatppuccinColor::Base));
        // A popup can extend past the final explicit cell in a short editor
        // row. Preserve that row's implicit tail before painting the float;
        // otherwise the terminal diff uses the popup border as the style for
        // ClearToEndOfLine and floods everything to its right with that color.
        for target in rows.iter_mut().skip(row).take(height) {
            materialize_row_tail(target, grid.width);
        }
        draw_popup_frame(&mut rows, Rect { column, row, width, height }, base_style, border_style, &popup.title);
        for (offset, line) in lines.iter().skip(scroll).take(visible_lines).enumerate() {
            paint_text(&mut rows[row + 1 + offset], column + 1, inner_width, &line.text, base_style);
            paint_decorated_popup_text(&mut rows[row + 1 + offset], column + 1, line, &popup.decorations, base_style, self.tab_width);
        }
        let cursor = popup_cursor.map_or(cursor, |(cursor_row, cursor_column)| {
            (column.saturating_add(1).saturating_add(cursor_column), row.saturating_add(1).saturating_add(cursor_row - scroll))
        });
        self.finish_overlay(grid, rows, cursor)
    }

    /// Composites a terminal-emulator surface into the docked right pane.
    pub fn apply_terminal_sidebar(&mut self, grid: DesiredGrid, sidebar: TerminalSidebar<'_>) -> DesiredGrid {
        let Some(column) = self.terminal_sidebar_column() else {
            return grid;
        };
        let inner_width = self.width.saturating_sub(column.saturating_add(1));
        let mut rows = overlay_rows(&grid);
        for row in &mut rows {
            materialize_row_tail(row, column);
        }
        let border = CellStyle::rgb(self.theme.color(CatppuccinColor::Blue), self.theme.color(CatppuccinColor::Base));
        let terminal_base = CellStyle::rgb(self.theme.color(CatppuccinColor::Text), self.theme.color(CatppuccinColor::Base));
        for (row_index, row) in rows.iter_mut().enumerate() {
            paint_text(row, column, 1, "│", border);
            replace_cells(
                row,
                column.saturating_add(1),
                inner_width,
                sidebar.rows.get(row_index).map_or(&[] as &[Cell], |source| source.cells.as_slice()).iter().cloned(),
                terminal_base,
            );
        }
        let cursor = if sidebar.focused {
            (column.saturating_add(1).saturating_add(sidebar.cursor.0.min(inner_width.saturating_sub(1))), sidebar.cursor.1.min(self.height.saturating_sub(1)))
        } else {
            grid.cursor
        };
        self.finish_overlay(grid, rows, cursor)
    }

    /// Starting column of the docked terminal surface for this terminal size.
    pub fn terminal_sidebar_column(&self) -> Option<usize> {
        Self::terminal_sidebar_column_for_size(self.width, self.height)
    }

    pub fn terminal_sidebar_column_for_size(width: usize, height: usize) -> Option<usize> {
        if width < 36 || height < 6 {
            return None;
        }
        let preferred = width.saturating_mul(2).checked_div(5).unwrap_or(32);
        let sidebar_width = preferred.clamp(32, 64).min(width.saturating_sub(12));
        Some(width.saturating_sub(sidebar_width))
    }

    /// The default nvim-dap-ui layout: scopes/breakpoints/stacks/watches on
    /// the left and REPL/console panels across the bottom.
    pub fn apply_debug_overlay(&mut self, grid: DesiredGrid, debug: &DebugOverlay) -> DesiredGrid {
        if self.width < 30 || self.height < 12 {
            return grid;
        }
        let mut rows = overlay_rows(&grid);
        let status_rows = 1;
        let usable_height = self.height.saturating_sub(status_rows);
        let bottom_height = usable_height.clamp(6, 10);
        let upper_height = usable_height.saturating_sub(bottom_height).max(4);
        let left_width = self.width.saturating_mul(3).checked_div(10).unwrap_or(30).clamp(24, 40);
        for row in rows.iter_mut().take(upper_height.min(usable_height)) {
            let source = std::mem::take(&mut row.cells);
            pad_row_to(row, left_width);
            row.cells.extend(source);
            truncate_row(row, self.width);
        }
        let mut cursor = grid.cursor;
        if cursor.1 < upper_height {
            cursor.0 = cursor.0.saturating_add(left_width).min(self.width - 1);
        }
        let base_style = CellStyle::rgb(self.theme.color(CatppuccinColor::Text), self.theme.color(CatppuccinColor::Mantle));
        let border_style = CellStyle::rgb(self.theme.color(CatppuccinColor::Blue), self.theme.color(CatppuccinColor::Base));
        let panel_height = (upper_height / 4).max(3);
        for (index, panel) in debug.panels[..4].iter().enumerate() {
            let row = index.saturating_mul(panel_height);
            let height = if index == 3 { upper_height.saturating_sub(row).max(3) } else { panel_height };
            draw_text_panel(&mut rows, Rect { column: 0, row, width: left_width, height }, panel.title, &panel.text, base_style, border_style);
        }
        let right_width = self.width.saturating_sub(left_width);
        let panel_width = right_width / 2;
        for (index, panel) in debug.panels[4..].iter().enumerate() {
            let column = left_width + index * panel_width;
            let width = if index == 1 { self.width.saturating_sub(column) } else { panel_width };
            draw_text_panel(&mut rows, Rect { column, row: upper_height, width, height: bottom_height }, panel.title, &panel.text, base_style, border_style);
        }
        self.finish_overlay(grid, rows, cursor)
    }

    /// Paints AceJump-style labels directly over visible matches in the active
    /// pane using the same split, gutter, tab, and wrap geometry as the buffer.
    pub fn apply_ace_jump_overlay(&mut self, grid: DesiredGrid, model: &ClientViewModel, frame: &EngineFrame, overlay: &AceJumpOverlay) -> DesiredGrid {
        if overlay.targets.is_empty() {
            return grid;
        }
        let tab = model.active_tab();
        let mut panes = Vec::new();
        split_rectangles(&tab.root, Rect { column: 0, row: 0, width: self.workspace_width(), height: self.height.saturating_sub(1).max(1) }, &mut panes);
        let Some((_, rectangle)) = panes.into_iter().find(|(window_id, _)| *window_id == tab.active_window) else {
            return grid;
        };
        let Some(window) = find_window(&tab.root, tab.active_window) else {
            return grid;
        };
        let gutter = self.editor_gutter(rectangle.width);
        let content_width = rectangle.width.saturating_sub(gutter).max(1);
        let start_byte = frame.text.byte_of_line(window.top_line);
        let end_byte = frame.text.byte_of_line(window.top_line.saturating_add(rectangle.height.max(1)));
        let mut rows = overlay_rows(&grid);
        let wanted =
            overlay.targets.iter().filter(|target| target.byte >= start_byte).map(|target| (target.byte, target.label.as_ref())).collect::<BTreeMap<_, _>>();
        let style = CellStyle::rgb(self.theme.color(CatppuccinColor::Base), self.theme.color(CatppuccinColor::Peach)).with_bold();
        let mut byte = start_byte;
        let mut screen_row = 0_usize;
        let mut screen_column = 0_usize;
        for grapheme in frame.text.slice(start_byte..end_byte).graphemes(true) {
            if let Some(label) = wanted.get(&byte)
                && screen_row < rectangle.height
            {
                let row = rectangle.row.saturating_add(screen_row);
                let column = rectangle.column.saturating_add(gutter).saturating_add(screen_column);
                let available = rectangle.column.saturating_add(rectangle.width).saturating_sub(column);
                if let Some(target) = rows.get_mut(row) {
                    paint_text(target, column, display_width(label, self.tab_width).min(available), label, style);
                }
            }
            advance_visual_grapheme(&mut screen_row, &mut screen_column, content_width, self.tab_width, grapheme);
            byte = byte.saturating_add(grapheme.len());
            if screen_row >= rectangle.height {
                break;
            }
        }
        let cursor = grid.cursor;
        self.finish_overlay(grid, rows, cursor)
    }

    pub fn apply_status_overlay(&mut self, grid: DesiredGrid, status: &StatusOverlay) -> DesiredGrid {
        if self.height == 0 {
            return grid;
        }
        let width = grid.width;
        let height = grid.height;
        let cursor = grid.cursor;
        let raster_overlay = grid.raster_overlay;
        let mut rows = grid.rows;
        let row_index = self.height - 1;
        let row = self.cached_status_rows.iter().rev().find(|(cached, _)| cached == status).map(|(_, row)| Arc::clone(row)).unwrap_or_else(|| {
            let base_style = CellStyle::rgb(self.theme.color(CatppuccinColor::Subtext1), self.theme.color(CatppuccinColor::Mantle));
            let mut row = CellRow { cells: Vec::with_capacity(self.width) };
            let mut left_column = 0;
            for segment in &status.left {
                let width = display_width(&segment.text, 1).min(self.width.saturating_sub(left_column));
                row.cells.extend(row_from_text(&segment.text, width, segment.style, 1).cells);
                left_column = left_column.saturating_add(width);
            }
            let right_width = status.right.iter().map(|segment| display_width(&segment.text, 1)).sum::<usize>().min(self.width.saturating_sub(left_column));
            let mut right_column = self.width.saturating_sub(right_width);
            row.cells.extend((left_column..right_column).map(|_| Cell { grapheme: single_byte_grapheme(b' '), width: 1, style: base_style }));
            for segment in &status.right {
                let width = display_width(&segment.text, 1).min(self.width.saturating_sub(right_column));
                row.cells.extend(row_from_text(&segment.text, width, segment.style, 1).cells);
                right_column = right_column.saturating_add(width);
            }
            row.cells.extend((right_column..self.width).map(|_| Cell { grapheme: single_byte_grapheme(b' '), width: 1, style: base_style }));
            apply_theme_to_rows(std::slice::from_mut(&mut row), self.theme);
            let row = Arc::new(row);
            self.cached_status_rows.push((status.clone(), Arc::clone(&row)));
            const STATUS_ROW_CACHE_CAPACITY: usize = 8;
            if self.cached_status_rows.len() > STATUS_ROW_CACHE_CAPACITY {
                self.cached_status_rows.remove(0);
            }
            row
        });
        if let Some(target) = rows.get_mut(row_index) {
            *target = row;
        }
        self.epoch = self.epoch.saturating_add(1);
        DesiredGrid { epoch: self.epoch, width, height, rows, cursor, raster_overlay }
    }

    fn finish_overlay(&mut self, grid: DesiredGrid, rows: Vec<CellRow>, cursor: (usize, usize)) -> DesiredGrid {
        self.epoch = self.epoch.saturating_add(1);
        DesiredGrid {
            epoch: self.epoch,
            width: grid.width,
            height: grid.height,
            rows: rows.into_iter().map(Arc::new).collect(),
            cursor,
            raster_overlay: grid.raster_overlay,
        }
    }
}

fn overlay_rows(grid: &DesiredGrid) -> Vec<CellRow> {
    grid.rows.iter().map(|row| row.as_ref().clone()).collect()
}

#[derive(Debug, Clone, Copy)]
struct PickerGeometry {
    width: usize,
    height: usize,
    column: usize,
    row: usize,
    preview_visible: bool,
    inner_width: usize,
    result_width: usize,
    preview_width: usize,
    prompt_row: usize,
    content_start: usize,
    content_height: usize,
}

impl PickerGeometry {
    const fn rectangle(self) -> Rect {
        Rect { column: self.column, row: self.row, width: self.width, height: self.height }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    column: usize,
    row: usize,
    width: usize,
    height: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RectSplit {
    first: Rect,
    border: Rect,
    second: Rect,
    border_grapheme: &'static str,
}

impl Rect {
    fn split(self, axis: SplitAxis, ratio_milli: u16) -> RectSplit {
        let extent = match axis {
            SplitAxis::Vertical => self.width,
            SplitAxis::Horizontal => self.height,
        };
        let available = extent.saturating_sub(1);
        let first_extent =
            available.saturating_mul(usize::from(ratio_milli)).checked_div(1000).unwrap_or_default().clamp(1, available.saturating_sub(1).max(1));
        match axis {
            SplitAxis::Vertical => RectSplit {
                first: Rect { width: first_extent, ..self },
                border: Rect { column: self.column + first_extent, width: 1, ..self },
                second: Rect { column: self.column + first_extent + 1, width: available.saturating_sub(first_extent), ..self },
                border_grapheme: "│",
            },
            SplitAxis::Horizontal => RectSplit {
                first: Rect { height: first_extent, ..self },
                border: Rect { row: self.row + first_extent, height: 1, ..self },
                second: Rect { row: self.row + first_extent + 1, height: available.saturating_sub(first_extent), ..self },
                border_grapheme: "─",
            },
        }
    }
}

fn workspace_pane_source<'a>(
    model: &'a ClientViewModel,
    frames: &'a [(BufferId, EngineFrame)],
    decorations: &'a [(BufferId, SharedDecorations)],
    line_decorations: &'a [(BufferId, Vec<LineDecoration>)],
    window_id: WindowId,
) -> Option<WorkspacePaneSource<'a>> {
    let window = find_window(&model.active_tab().root, window_id)?;
    let frame = frames.iter().find(|(buffer_id, _)| *buffer_id == window.buffer_id).map(|(_, frame)| frame)?;
    static EMPTY_DECORATIONS: std::sync::LazyLock<SharedDecorations> = std::sync::LazyLock::new(|| Arc::new(Vec::new()));
    let decorations = decorations.iter().find(|(buffer_id, _)| *buffer_id == window.buffer_id).map_or(&*EMPTY_DECORATIONS, |(_, spans)| spans);
    let line_decorations = line_decorations.iter().find(|(buffer_id, _)| *buffer_id == window.buffer_id).map_or(&[][..], |(_, spans)| spans.as_slice());
    Some(WorkspacePaneSource { window, frame, decorations, line_decorations })
}

fn merge_pane_rows(rows: &mut [CellRow], rectangle: Rect, pane_rows: &[Arc<CellRow>]) {
    for (pane_row, source) in pane_rows.iter().enumerate() {
        let Some(target) = rows.get_mut(rectangle.row + pane_row) else {
            continue;
        };
        pad_row_to(target, rectangle.column);
        target.cells.extend(source.cells.iter().cloned());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PopupPlacement {
    column: usize,
    row: usize,
}

fn text_popup_placement(grid: &DesiredGrid, width: usize, height: usize) -> PopupPlacement {
    // Keep the right border out of the terminal's final column. Some terminal
    // emulators defer wrapping after that cell, which can make a correct final
    // border appear clipped or move it onto the following row.
    let right = grid.width.saturating_sub(width.saturating_add(1));
    // The last row normally belongs to the status line. Reserving it also
    // keeps a float visually separate from the terminal edge when status is
    // hidden.
    let bottom = grid.height.saturating_sub(1).saturating_sub(height);
    let below_cursor = grid.cursor.1.saturating_add(1).min(bottom);
    let above_cursor = grid.cursor.1.saturating_sub(height).min(bottom);
    let right_of_cursor = grid.cursor.0.saturating_add(1).min(right);
    let left_of_cursor = grid.cursor.0.saturating_sub(width).min(right);

    // Stable edge placements come first, so equal amounts of empty space do
    // not make a visible float follow ordinary cursor movement. Cursor-local
    // placements remain useful when both sides of the editor contain text.
    let cursor_local_candidates = [
        PopupPlacement { column: right_of_cursor, row: below_cursor },
        PopupPlacement { column: left_of_cursor, row: below_cursor },
        PopupPlacement { column: right_of_cursor, row: above_cursor },
        PopupPlacement { column: left_of_cursor, row: above_cursor },
    ];

    (0..=bottom)
        .map(|row| PopupPlacement { column: right, row })
        .chain((0..=bottom).map(|row| PopupPlacement { column: 0, row }))
        .chain(cursor_local_candidates)
        .enumerate()
        .min_by_key(|(preference, placement)| {
            (
                occupied_cells_under_popup(grid, *placement, width, height),
                grid.cursor.0 >= placement.column
                    && grid.cursor.0 < placement.column.saturating_add(width)
                    && grid.cursor.1 >= placement.row
                    && grid.cursor.1 < placement.row.saturating_add(height),
                *preference,
            )
        })
        .map_or(PopupPlacement { column: 0, row: 0 }, |(_, placement)| placement)
}

fn popup_max_inner_width(terminal_width: usize) -> usize {
    terminal_width.saturating_sub(4).clamp(4, 80)
}

fn occupied_cells_under_popup(grid: &DesiredGrid, placement: PopupPlacement, width: usize, height: usize) -> usize {
    let popup_end = placement.column.saturating_add(width);
    grid.rows
        .iter()
        .skip(placement.row)
        .take(height)
        .map(|row| {
            let mut display_column = 0_usize;
            let mut occupied = 0_usize;
            for cell in &row.cells {
                let cell_start = display_column;
                let cell_end = cell_start.saturating_add(usize::from(cell.width));
                display_column = cell_end;
                if cell_start >= popup_end {
                    break;
                }
                if !cell.grapheme.chars().all(char::is_whitespace) {
                    occupied = occupied.saturating_add(cell_end.min(popup_end).saturating_sub(cell_start.max(placement.column)));
                }
            }
            occupied
        })
        .sum()
}

fn apply_theme_to_rows(rows: &mut [CellRow], theme: CatppuccinPalette) {
    if theme == CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha) {
        return;
    }
    for cell in rows.iter_mut().flat_map(|row| &mut row.cells) {
        cell.style.foreground = cell.style.foreground.map(|color| theme.remap_from_mocha(color));
        cell.style.background = cell.style.background.map(|color| theme.remap_from_mocha(color));
    }
}

fn ensure_row_backgrounds(rows: &mut [CellRow]) {
    for row in rows {
        if row.cells.is_empty() {
            row.cells.push(Cell { grapheme: " ".into(), width: 1, style: CellStyle::default() });
        }
    }
}

fn paint_text(row: &mut CellRow, column: usize, width: usize, text: &str, style: CellStyle) {
    replace_cells(row, column, width, row_from_text(text, width, style, 1).cells, style);
}

fn replace_cells(row: &mut CellRow, column: usize, width: usize, replacement: impl IntoIterator<Item = Cell>, fill_style: CellStyle) {
    if width == 0 {
        return;
    }
    let end = column.saturating_add(width);
    let mut before = Vec::new();
    let mut after = Vec::new();
    let mut display_column = 0_usize;
    for cell in &row.cells {
        let cell_end = display_column.saturating_add(usize::from(cell.width));
        if cell_end <= column {
            before.push(cell.clone());
        } else if display_column >= end {
            after.push(cell.clone());
        }
        display_column = cell_end;
    }
    let before_width = before.iter().map(|cell| usize::from(cell.width)).sum::<usize>();
    before.extend((before_width..column).map(|_| Cell { grapheme: " ".into(), width: 1, style: fill_style }));
    let mut replacement_width = 0_usize;
    for cell in replacement {
        let next = replacement_width.saturating_add(usize::from(cell.width));
        if next > width {
            break;
        }
        before.push(cell);
        replacement_width = next;
    }
    before.extend((replacement_width..width).map(|_| Cell { grapheme: " ".into(), width: 1, style: fill_style }));
    before.extend(after);
    row.cells = before;
}

fn truncate_row(row: &mut CellRow, width: usize) {
    let mut used = 0_usize;
    let mut stopped = false;
    row.cells.retain(|cell| {
        if stopped {
            return false;
        }
        let next = used.saturating_add(usize::from(cell.width));
        let keep = next <= width;
        if keep {
            used = next;
        } else {
            stopped = true;
        }
        keep
    });
}

fn draw_popup_frame(rows: &mut [CellRow], rectangle: Rect, base_style: CellStyle, border_style: CellStyle, title: &str) {
    let Rect { column, row, width, height } = rectangle;
    if width < 2 || height < 2 || row.saturating_add(height) > rows.len() {
        return;
    }
    for target in rows.iter_mut().skip(row).take(height) {
        paint_text(target, column, width, "", base_style);
    }
    paint_text(&mut rows[row], column, width, &format!("╭{}╮", "─".repeat(width.saturating_sub(2))), border_style);
    paint_text(&mut rows[row + height - 1], column, width, &format!("╰{}╯", "─".repeat(width.saturating_sub(2))), border_style);
    for target in rows.iter_mut().take(row + height - 1).skip(row + 1) {
        paint_text(target, column, 1, "│", border_style);
        paint_text(target, column + width - 1, 1, "│", border_style);
    }
    if !title.is_empty() {
        let title = format!(" {title} ");
        paint_text(&mut rows[row], column + 2, display_width(&title, 1).min(width.saturating_sub(4)), &title, border_style.with_bold());
    }
}

fn draw_text_panel(rows: &mut [CellRow], rectangle: Rect, title: &str, text: &str, base_style: CellStyle, border_style: CellStyle) {
    let Rect { column, row, width, height } = rectangle;
    if width < 4 || height < 3 {
        return;
    }
    draw_popup_frame(rows, rectangle, base_style, border_style, title);
    for (offset, line) in text.lines().take(height.saturating_sub(2)).enumerate() {
        paint_text(&mut rows[row + 1 + offset], column + 1, width.saturating_sub(2), line, base_style);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WrappedPopupLine {
    text: String,
    source_start: usize,
    source_line: usize,
}

fn wrap_popup_text(text: &str, width: usize, tab_width: usize) -> Vec<WrappedPopupLine> {
    let mut output = Vec::new();
    let mut source_offset = 0;
    for (source_line_number, source_with_newline) in text.split_inclusive('\n').enumerate() {
        let without_newline = source_with_newline.strip_suffix('\n').unwrap_or(source_with_newline);
        let source_line = without_newline.strip_suffix('\r').unwrap_or(without_newline);
        output.extend(wrap_popup_line(source_line, source_offset, source_line_number, width, tab_width));
        source_offset += source_with_newline.len();
    }
    if output.is_empty() {
        output.push(WrappedPopupLine { text: String::new(), source_start: 0, source_line: 0 });
    }
    output
}

fn wrap_popup_line(source: &str, source_offset: usize, source_line: usize, width: usize, tab_width: usize) -> Vec<WrappedPopupLine> {
    if source.is_empty() || display_width(source, tab_width) <= width {
        return vec![WrappedPopupLine { text: source.to_owned(), source_start: source_offset, source_line }];
    }
    let mut output = Vec::new();
    let mut start = 0;
    while start < source.len() {
        let (display_end, next) = popup_wrap_boundary(source, start, width, tab_width);
        output.push(WrappedPopupLine { text: source[start..display_end].trim_end().to_owned(), source_start: source_offset + start, source_line });
        start = skip_grapheme_whitespace(source, next);
    }
    output
}

fn popup_wrap_boundary(source: &str, start: usize, width: usize, tab_width: usize) -> (usize, usize) {
    let mut used = 0_usize;
    let mut end = start;
    let mut whitespace = None;
    for (relative, grapheme) in source[start..].grapheme_indices(true) {
        let grapheme_start = start + relative;
        let grapheme_end = grapheme_start + grapheme.len();
        let grapheme_width = if grapheme == "\t" { tab_width.max(1) - (used % tab_width.max(1)) } else { display_width(grapheme, 1) };
        if used.saturating_add(grapheme_width) > width && end > start {
            break;
        }
        used = used.saturating_add(grapheme_width);
        end = grapheme_end;
        if grapheme.chars().all(char::is_whitespace) {
            whitespace = Some((grapheme_start, grapheme_end));
        }
        if used >= width {
            break;
        }
    }
    let end = if end == start { source[start..].graphemes(true).next().map_or(source.len(), |grapheme| start + grapheme.len()) } else { end };
    match whitespace {
        Some((space_start, space_end)) if space_start > start && end < source.len() => (space_start, space_end),
        _ => (end, end),
    }
}

fn skip_grapheme_whitespace(source: &str, mut start: usize) -> usize {
    while let Some(grapheme) = source[start..].graphemes(true).next() {
        if !grapheme.chars().all(char::is_whitespace) {
            break;
        }
        start += grapheme.len();
    }
    start
}

fn paint_decorated_popup_text(
    row: &mut CellRow,
    column: usize,
    line: &WrappedPopupLine,
    decorations: &[DecorationSpan],
    base_style: CellStyle,
    tab_width: usize,
) {
    let mut display_column = column;
    for (relative, grapheme) in line.text.grapheme_indices(true) {
        let range = line.source_start + relative..line.source_start + relative + grapheme.len();
        if let Some(overlay) = decoration_overlay_style(decorations, range) {
            let width = if grapheme == "\t" { tab_width - ((display_column - column) % tab_width) } else { display_width(grapheme, 1) };
            apply_style_to_columns(row, display_column, display_column.saturating_add(width), merge_styles(base_style, overlay));
            display_column = display_column.saturating_add(width);
        } else {
            display_column = display_column.saturating_add(display_width(grapheme, tab_width));
        }
    }
}

fn apply_style_to_columns(row: &mut CellRow, start: usize, end: usize, style: CellStyle) {
    let mut column = 0_usize;
    for cell in &mut row.cells {
        let cell_end = column.saturating_add(usize::from(cell.width));
        if column < end && start < cell_end {
            cell.style = style;
        }
        column = cell_end;
    }
}

const fn merge_styles(base: CellStyle, overlay: CellStyle) -> CellStyle {
    CellStyle {
        attributes: base.attributes | overlay.attributes,
        foreground: if overlay.foreground.is_some() { overlay.foreground } else { base.foreground },
        background: if overlay.background.is_some() { overlay.background } else { base.background },
    }
}

fn split_rectangles(tree: &SplitTree, rectangle: Rect, panes: &mut Vec<(WindowId, Rect)>) {
    match tree {
        SplitTree::Window(window) => panes.push((window.id, rectangle)),
        SplitTree::Split { axis, ratio_milli, first, second } => {
            let split = rectangle.split(*axis, *ratio_milli);
            split_rectangles(first, split.first, panes);
            split_rectangles(second, split.second, panes);
        }
    }
}

fn pad_row_to(row: &mut CellRow, column: usize) {
    let width: usize = row.cells.iter().map(|cell| usize::from(cell.width)).sum();
    for _ in width..column {
        row.cells.push(Cell { grapheme: " ".into(), width: 1, style: CellStyle::default() });
    }
}

fn materialize_row_tail(row: &mut CellRow, width: usize) {
    let current = row.cells.iter().map(|cell| usize::from(cell.width)).sum::<usize>();
    let style = row.cells.last().map_or_else(CellStyle::default, |cell| cell.style);
    row.cells.extend((current..width).map(|_| Cell { grapheme: " ".into(), width: 1, style }));
}

fn mark_color_column(rows: &mut [CellRow], column: usize, width: usize, color: RgbColor) {
    if column >= width {
        return;
    }
    for row in rows {
        let mut display_column = 0_usize;
        for cell in &mut row.cells {
            let end = display_column.saturating_add(usize::from(cell.width));
            // Treat the configured text limit as the start of an overflow
            // region. Short rows already paint their blank tail this color via
            // ClearToEndOfLine; applying it to every occupied cell at or past
            // the limit keeps long and wrapped rows on the same vertical edge.
            if column < end {
                cell.style.background = Some(CellColor::Rgb(color));
            }
            display_column = end;
        }
        if display_column <= column {
            pad_row_to(row, column);
            row.cells.push(Cell { grapheme: " ".into(), width: 1, style: CellStyle::default().with_background(CellColor::Rgb(color)) });
        }
    }
}

fn put_border(rows: &mut [CellRow], column: usize, row_index: usize, grapheme: &str) {
    let Some(row) = rows.get_mut(row_index) else {
        return;
    };
    let border = Cell { grapheme: grapheme.into(), width: 1, style: CellStyle::default().with_reverse() };
    let mut display_column = 0;
    for cell in &mut row.cells {
        if display_column == column {
            *cell = border;
            return;
        }
        display_column += usize::from(cell.width);
    }
    pad_row_to(row, column);
    row.cells.push(border);
}

fn draw_split_borders(tree: &SplitTree, column: usize, row: usize, width: usize, height: usize, rows: &mut [CellRow]) {
    let SplitTree::Split { axis, ratio_milli, first, second } = tree else {
        return;
    };
    let split = Rect { column, row, width, height }.split(*axis, *ratio_milli);
    for row in split.border.row..split.border.row.saturating_add(split.border.height) {
        for column in split.border.column..split.border.column.saturating_add(split.border.width) {
            put_border(rows, column, row, split.border_grapheme);
        }
    }
    draw_split_borders(first, split.first.column, split.first.row, split.first.width, split.first.height, rows);
    draw_split_borders(second, split.second.column, split.second.row, split.second.width, split.second.height, rows);
}

fn byte_at_visual_cell(text: &FrameText, document_start: usize, width: usize, tab_width: usize, target_column: usize, target_row: usize) -> Option<usize> {
    let start_line = text.line_of_byte(document_start);
    let end = text.byte_of_line(start_line.saturating_add(target_row).saturating_add(1));
    let visible = text.slice(document_start..end);
    let width = width.max(1);
    let tab_width = tab_width.max(1);
    let mut absolute_byte = document_start;
    let mut row = 0_usize;
    let mut column = 0_usize;
    let mut last_grapheme_on_row = None;

    for grapheme in visible.graphemes(true) {
        let grapheme_start = absolute_byte;
        absolute_byte = absolute_byte.saturating_add(grapheme.len());
        if grapheme == "\n" {
            if row == target_row {
                return Some(last_grapheme_on_row.unwrap_or(grapheme_start));
            }
            row = row.saturating_add(1);
            column = 0;
            last_grapheme_on_row = None;
            if row > target_row {
                return None;
            }
            continue;
        }
        let displayed = if grapheme == "\t" { vec![" ".to_owned(); tab_width - (column % tab_width)] } else { escape_grapheme(grapheme) };
        for cell in displayed {
            let cell_width = UnicodeWidthStr::width(cell.as_str()).max(1);
            if column.saturating_add(cell_width) > width && column > 0 {
                if row == target_row {
                    return Some(last_grapheme_on_row.unwrap_or(grapheme_start));
                }
                row = row.saturating_add(1);
                column = 0;
                if row > target_row {
                    return None;
                }
            }
            if row == target_row && target_column >= column && target_column < column.saturating_add(cell_width) {
                return Some(grapheme_start);
            }
            column = column.saturating_add(cell_width);
            last_grapheme_on_row = Some(grapheme_start);
        }
    }
    (row <= target_row).then_some(last_grapheme_on_row.unwrap_or(absolute_byte))
}

struct GridBuilder {
    width: usize,
    height: usize,
    tab_width: usize,
    document_start: usize,
    rows: Vec<CellRow>,
    row: usize,
    column: usize,
    cursor: Option<(usize, usize)>,
    row_lines: Vec<Option<usize>>,
    logical_line: usize,
}

impl GridBuilder {
    fn new(width: usize, height: usize, tab_width: usize, document_start: usize, start_line: usize) -> Self {
        let mut rows = Vec::with_capacity(height.max(1));
        rows.push(CellRow { cells: Vec::with_capacity(width.min(256)) });
        let mut row_lines = Vec::with_capacity(height.max(1));
        row_lines.push(Some(start_line));
        Self {
            width: width.max(1),
            height: height.max(1),
            tab_width: tab_width.max(1),
            document_start,
            rows,
            row: 0,
            column: 0,
            cursor: None,
            row_lines,
            logical_line: start_line,
        }
    }

    fn push_grapheme_document(&mut self, visible: &str, cursor_byte: usize, decorations: &[DecorationSpan]) {
        if visible.is_ascii() {
            self.push_ascii_document(visible.as_bytes(), cursor_byte, decorations);
            return;
        }
        let mut absolute_byte = self.document_start;
        let mut decoration_resolver = DecorationResolver::new_at(decorations, self.document_start);
        if cursor_byte < self.document_start {
            self.cursor = Some((0, 0));
        }
        for grapheme in visible.graphemes(true) {
            let grapheme_start = absolute_byte;
            self.mark_cursor(absolute_byte, cursor_byte);
            absolute_byte += grapheme.len();
            let style = decoration_resolver.style_or_default(grapheme_start..absolute_byte);
            if !self.push_document_grapheme(grapheme, style) {
                break;
            }
        }
        self.mark_cursor(absolute_byte, cursor_byte);
    }

    fn push_ascii_document(&mut self, visible: &[u8], cursor_byte: usize, decorations: &[DecorationSpan]) {
        let mut absolute_byte = self.document_start;
        let mut decoration_resolver = DecorationResolver::new_at(decorations, self.document_start);
        if cursor_byte < self.document_start {
            self.cursor = Some((0, 0));
        }
        for byte in visible.iter().copied() {
            self.mark_cursor(absolute_byte, cursor_byte);
            let style = decoration_resolver.style_or_default(absolute_byte..absolute_byte.saturating_add(1));
            absolute_byte = absolute_byte.saturating_add(1);
            if !self.push_document_ascii(byte, style) {
                break;
            }
        }
        self.mark_cursor(absolute_byte, cursor_byte);
    }

    fn mark_cursor(&mut self, absolute_byte: usize, cursor_byte: usize) {
        if absolute_byte == cursor_byte && self.row < self.height {
            self.cursor = Some((self.column, self.row));
        }
    }

    fn push_document_grapheme(&mut self, grapheme: &str, style: CellStyle) -> bool {
        match grapheme {
            "\n" => {
                self.logical_line = self.logical_line.saturating_add(1);
                self.next_row(Some(self.logical_line))
            }
            "\t" => self.push_tab(style),
            _ if grapheme.chars().all(|character| !character.is_control()) => self.push_cell(grapheme, style),
            _ => escape_grapheme(grapheme).iter().all(|escaped| self.push_cell(escaped, style)),
        }
    }

    fn push_tab(&mut self, style: CellStyle) -> bool {
        let spaces = self.tab_width - (self.column % self.tab_width);
        (0..spaces).all(|_| self.push_ascii_cell(b' ', style))
    }

    fn push_document_ascii(&mut self, byte: u8, style: CellStyle) -> bool {
        match byte {
            b'\n' => {
                self.logical_line = self.logical_line.saturating_add(1);
                self.next_row(Some(self.logical_line))
            }
            b'\t' => self.push_tab(style),
            0x20..=0x7e => self.push_ascii_cell(byte, style),
            _ => {
                let escaped = match byte {
                    0x00 => b'@',
                    0x01..=0x1a | 0x1c..=0x1f => byte.saturating_add(64),
                    0x1b => b'[',
                    _ => b'?',
                };
                self.push_ascii_cell(b'^', style) && self.push_ascii_cell(escaped, style)
            }
        }
    }

    fn push_ascii_cell(&mut self, byte: u8, style: CellStyle) -> bool {
        if self.column == self.width && !self.next_row(None) {
            return false;
        }
        if self.row >= self.height {
            return false;
        }
        if let Some(row) = self.rows.get_mut(self.row) {
            row.cells.push(Cell { grapheme: single_byte_grapheme(byte), width: 1, style });
        }
        self.column += 1;
        true
    }

    fn push_cell(&mut self, grapheme: &str, style: CellStyle) -> bool {
        let width = UnicodeWidthStr::width(grapheme).max(1);
        if self.column + width > self.width && self.column > 0 && !self.next_row(None) {
            return false;
        }
        if self.row >= self.height {
            return false;
        }
        if let Some(row) = self.rows.get_mut(self.row) {
            row.cells.push(Cell { grapheme: grapheme.into(), width: u8::try_from(width).unwrap_or(u8::MAX), style });
        }
        self.column += width;
        true
    }

    fn next_row(&mut self, logical_line: Option<usize>) -> bool {
        self.row += 1;
        self.column = 0;
        if self.row >= self.height {
            return false;
        }
        self.rows.push(CellRow { cells: Vec::with_capacity(self.width.min(256)) });
        self.row_lines.push(logical_line);
        true
    }
}

struct DecorationResolver<'a> {
    decorations: &'a [DecorationSpan],
    starts: Option<Vec<usize>>,
    next: usize,
    active: Vec<usize>,
    cached_style: CellStyle,
    valid_until: usize,
}

impl<'a> DecorationResolver<'a> {
    #[cfg(test)]
    fn new(decorations: &'a [DecorationSpan]) -> Self {
        Self::new_at(decorations, 0)
    }

    fn new_at(decorations: &'a [DecorationSpan], start: usize) -> Self {
        let starts = (!decorations.windows(2).all(|pair| pair[0].range.start <= pair[1].range.start)).then(|| {
            let mut starts = (0..decorations.len()).collect::<Vec<_>>();
            starts.sort_by_key(|index| (decorations[*index].range.start, *index));
            starts
        });
        let next = starts.as_ref().map_or_else(
            || decorations.partition_point(|span| span.range.start < start),
            |starts| starts.partition_point(|index| decorations[*index].range.start < start),
        );
        let active = (0..next)
            .filter_map(|position| {
                let index = starts.as_ref().map_or(position, |starts| starts[position]);
                (decorations[index].range.end > start).then_some(index)
            })
            .collect();
        Self { decorations, starts, next, active, cached_style: CellStyle::default(), valid_until: 0 }
    }

    fn style(&mut self, range: Range<usize>) -> CellStyle {
        if range.end <= self.valid_until {
            return self.cached_style;
        }
        self.advance(&range);
        self.cached_style = decoration_style_from_indices(self.decorations, self.active.iter().copied(), range.clone());
        let next_start = self.next_decoration().map_or(usize::MAX, |index| self.decorations[index].range.start);
        let active_end = self.active.iter().map(|index| self.decorations[*index].range.end).min().unwrap_or(usize::MAX);
        self.valid_until = next_start.min(active_end).max(range.end);
        self.cached_style
    }

    fn style_or_default(&mut self, range: Range<usize>) -> CellStyle {
        if self.decorations.is_empty() { CellStyle::default() } else { self.style(range) }
    }

    fn overlapping_indices(&mut self, range: Range<usize>) -> &[usize] {
        self.advance(&range);
        &self.active
    }

    fn advance(&mut self, range: &Range<usize>) {
        self.active.retain(|index| self.decorations[*index].range.end > range.start);
        while let Some(index) = self.next_decoration()
            && self.decorations[index].range.start < range.end
        {
            self.active.push(index);
            self.next += 1;
        }
    }

    fn next_decoration(&self) -> Option<usize> {
        self.starts.as_ref().map_or_else(|| (self.next < self.decorations.len()).then_some(self.next), |starts| starts.get(self.next).copied())
    }
}

fn prepend_line_numbers(
    rows: &mut [CellRow],
    row_lines: &[Option<usize>],
    cursor_line: usize,
    width: usize,
    relative: bool,
    line_decorations: &[LineDecoration],
) {
    for (index, row) in rows.iter_mut().enumerate() {
        let prefix = line_number_prefix(row_lines.get(index).copied().flatten(), cursor_line, width, relative, line_decorations);
        let prefix_len = prefix.cells.len();
        row.cells.reserve(prefix_len);
        row.cells.extend(prefix.cells);
        row.cells.rotate_right(prefix_len);
    }
}

fn replace_line_number(row: &mut CellRow, logical_line: usize, cursor_line: usize, width: usize, relative: bool, line_decorations: &[LineDecoration]) {
    let prefix = line_number_prefix(Some(logical_line), cursor_line, width, relative, line_decorations);
    for (target, replacement) in row.cells.iter_mut().zip(prefix.cells) {
        *target = replacement;
    }
}

fn line_number_prefix(logical_line: Option<usize>, cursor_line: usize, width: usize, relative: bool, line_decorations: &[LineDecoration]) -> CellRow {
    let style = logical_line.and_then(|line| line_decorations.iter().rev().find(|decoration| decoration.line == line)).map_or(
        CellStyle::default().with_foreground(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Overlay1))),
        |decoration| decoration.style,
    );
    let mut cells = vec![Cell { grapheme: single_byte_grapheme(b' '), width: 1, style }; width];
    if let Some(line) = logical_line {
        let mut number = if relative && line != cursor_line { line.abs_diff(cursor_line) } else { line.saturating_add(1) };
        let mut reversed = [0_u8; 20];
        let mut digits = 0;
        loop {
            reversed[digits] = b'0' + u8::try_from(number % 10).unwrap_or(0);
            digits += 1;
            number /= 10;
            if number == 0 {
                break;
            }
        }
        let field = width.saturating_sub(1);
        let padding = field.saturating_sub(digits);
        for (offset, digit) in reversed[..digits].iter().rev().copied().enumerate() {
            if let Some(target) = cells.get_mut(padding.saturating_add(offset)) {
                target.grapheme = single_byte_grapheme(digit);
            }
        }
    }
    CellRow { cells }
}

fn logical_line_range(text: &FrameText, line: usize) -> Range<usize> {
    let start = text.byte_of_line(line);
    let end = text.byte_of_line(line.saturating_add(1));
    start..end.saturating_sub(usize::from(end > start && text.slice(end - 1..end).as_ref() == "\n"))
}

fn single_byte_grapheme(byte: u8) -> CellGrapheme {
    let mut encoded = [0; 4];
    CellGrapheme::new(char::from(byte).encode_utf8(&mut encoded))
}

#[cfg(test)]
fn decoration_style(decorations: &[DecorationSpan], range: Range<usize>) -> CellStyle {
    decoration_style_from_indices(decorations, 0..decorations.len(), range)
}

fn decoration_style_from_indices(decorations: &[DecorationSpan], indices: impl IntoIterator<Item = usize>, range: Range<usize>) -> CellStyle {
    resolve_decoration_style(decorations, indices, range, CellStyle::default()).0
}

fn decoration_overlay_style(decorations: &[DecorationSpan], range: Range<usize>) -> Option<CellStyle> {
    let (style, overlaps) = resolve_decoration_style(decorations, 0..decorations.len(), range, transparent_cell_style());
    overlaps.then_some(style)
}

fn resolve_decoration_style(
    decorations: &[DecorationSpan],
    indices: impl IntoIterator<Item = usize>,
    range: Range<usize>,
    mut style: CellStyle,
) -> (CellStyle, bool) {
    let mut foreground_priority = None::<(u32, usize)>;
    let mut background_priority = None::<(u32, usize)>;
    let mut overlaps = false;
    for order in indices {
        let decoration = &decorations[order];
        if decoration.range.start >= range.end || range.start >= decoration.range.end {
            continue;
        }
        overlaps = true;
        style.attributes |= decoration.style.attributes;
        let precedence = (decoration.priority, order);
        if let Some(foreground) = decoration.style.foreground
            && foreground_priority.is_none_or(|current| precedence >= current)
        {
            style.foreground = Some(foreground);
            foreground_priority = Some(precedence);
        }
        if let Some(background) = decoration.style.background
            && background_priority.is_none_or(|current| precedence >= current)
        {
            style.background = Some(background);
            background_priority = Some(precedence);
        }
    }
    (style, overlaps)
}

const fn transparent_cell_style() -> CellStyle {
    CellStyle { attributes: 0, foreground: None, background: None }
}

fn escape_grapheme(grapheme: &str) -> Vec<String> {
    if grapheme.chars().all(|character| !character.is_control()) {
        return vec![grapheme.to_owned()];
    }
    let mut escaped = Vec::new();
    for character in grapheme.chars() {
        match character {
            '\u{00}' => escaped.extend(["^".to_owned(), "@".to_owned()]),
            '\u{01}'..='\u{1a}' | '\u{1c}'..='\u{1f}' => {
                escaped.push("^".to_owned());
                let display = char::from_u32(u32::from(character) + 64).unwrap_or('?');
                escaped.push(display.to_string());
            }
            '\u{1b}' => escaped.extend(["^".to_owned(), "[".to_owned()]),
            '\u{7f}' => escaped.extend(["^".to_owned(), "?".to_owned()]),
            _ => escaped.push(character.to_string()),
        }
    }
    escaped
}

fn prompt_style(is_prompt: bool) -> CellStyle {
    if is_prompt {
        return CellStyle::default();
    }
    CellStyle::default().with_bold().with_reverse()
}

fn row_from_text(text: &str, width: usize, style: CellStyle, tab_width: usize) -> CellRow {
    if text.bytes().all(|byte| matches!(byte, 0x20..=0x7e)) {
        return CellRow { cells: text.bytes().take(width).map(|byte| Cell { grapheme: single_byte_grapheme(byte), width: 1, style }).collect() };
    }
    let mut row = CellRow { cells: Vec::with_capacity(width.min(text.len())) };
    let mut column = 0;
    for grapheme in text.graphemes(true) {
        if grapheme != "\t" && grapheme.chars().all(|character| !character.is_control()) {
            let cell_width = UnicodeWidthStr::width(grapheme).max(1);
            if column + cell_width > width {
                return row;
            }
            row.cells.push(Cell { grapheme: grapheme.into(), width: u8::try_from(cell_width).unwrap_or(u8::MAX), style });
            column += cell_width;
            continue;
        }
        let representations = if grapheme == "\t" { vec![" ".to_owned(); tab_width - (column % tab_width)] } else { escape_grapheme(grapheme) };
        for representation in representations {
            let cell_width = UnicodeWidthStr::width(representation.as_str()).max(1);
            if column + cell_width > width {
                return row;
            }
            row.cells.push(Cell { grapheme: representation.into(), width: u8::try_from(cell_width).unwrap_or(u8::MAX), style });
            column += cell_width;
        }
    }
    row
}

fn display_width(text: &str, tab_width: usize) -> usize {
    if text.bytes().all(|byte| matches!(byte, 0x20..=0x7e)) {
        return text.len();
    }
    let mut width = 0;
    for grapheme in text.graphemes(true) {
        if grapheme == "\t" {
            width += tab_width - (width % tab_width);
        } else {
            width += escape_grapheme(grapheme).iter().map(|value| UnicodeWidthStr::width(value.as_str()).max(1)).sum::<usize>();
        }
    }
    width
}

fn relative_decorations_from_indices(decorations: &[DecorationSpan], indices: &[usize], range: Range<usize>) -> Vec<DecorationSpan> {
    indices
        .iter()
        .map(|index| {
            let span = &decorations[*index];
            DecorationSpan::new(span.range.start.max(range.start) - range.start..span.range.end.min(range.end) - range.start, span.style, span.priority)
        })
        .collect()
}

fn relative_decorations_equal(decorations: &[DecorationSpan], indices: &[usize], range: Range<usize>, expected: &[DecorationSpan]) -> bool {
    indices.len() == expected.len()
        && indices.iter().zip(expected).all(|(index, expected)| {
            let span = &decorations[*index];
            span.style == expected.style
                && span.priority == expected.priority
                && span.range.start.max(range.start) - range.start == expected.range.start
                && span.range.end.min(range.end) - range.start == expected.range.end
        })
}

fn decoration_change_range(
    old: &[DecorationSpan],
    new: &[DecorationSpan],
    change: FrameTextChange,
    old_visible: Range<usize>,
    new_visible: Range<usize>,
) -> Range<usize> {
    let overlaps = |span: &&DecorationSpan, visible: &Range<usize>| span.range.start < visible.end && visible.start < span.range.end;
    let inside_old = |span: &&DecorationSpan| span.range.start < change.old_end && change.old_start < span.range.end;
    let inside_new = |span: &&DecorationSpan| span.range.start < change.new_end && change.new_start < span.range.end;
    let shift = |offset: usize| {
        if change.new_end >= change.old_end { offset.checked_add(change.new_end - change.old_end) } else { offset.checked_sub(change.old_end - change.new_end) }
    };
    let mut affected = change.new_start..change.new_end.max(change.new_start.saturating_add(1));
    let mut include = |span: Option<DecorationSpan>| {
        if let Some(span) = span {
            affected.start = affected.start.min(span.range.start);
            affected.end = affected.end.max(span.range.end);
        }
    };
    for span in old.iter().filter(|span| overlaps(span, &old_visible) && inside_old(span)) {
        let start = if span.range.start < change.old_start { span.range.start } else { change.new_start };
        let end = if span.range.end > change.old_end { shift(span.range.end).unwrap_or(change.new_end) } else { change.new_end };
        include(Some(DecorationSpan::new(start..end, span.style, span.priority)));
    }
    for span in new.iter().filter(|span| overlaps(span, &new_visible) && inside_new(span)) {
        include(Some((*span).clone()));
    }
    let mut old = old.iter().filter(|span| overlaps(span, &old_visible) && !inside_old(span)).filter_map(|span| {
        let mut mapped = span.clone();
        if mapped.range.start >= change.old_end {
            mapped.range = shift(mapped.range.start)?..shift(mapped.range.end)?;
        }
        Some(mapped)
    });
    let mut new = new.iter().filter(|span| overlaps(span, &new_visible) && !inside_new(span)).cloned();
    loop {
        match (old.next(), new.next()) {
            (None, None) => return affected,
            (Some(old), Some(new)) if old == new => {}
            (old, new) => {
                include(old);
                include(new);
                break;
            }
        }
    }
    loop {
        match (old.next_back(), new.next_back()) {
            (None, None) => return affected,
            (Some(old), Some(new)) if old == new => {}
            (old_span, new_span) => {
                include(old_span);
                include(new_span);
                old.for_each(|span| include(Some(span)));
                new.for_each(|span| include(Some(span)));
                return affected;
            }
        }
    }
}

fn decorations_match_range(left: &[DecorationSpan], right: &[DecorationSpan], range: &Range<usize>) -> bool {
    let overlaps = |span: &&DecorationSpan| span.range.start < range.end && range.start < span.range.end;
    left.iter().filter(overlaps).eq(right.iter().filter(overlaps))
}

fn single_line_change(old: &FrameText, new: &FrameText) -> Option<FrameTextChange> {
    if let Some(change) = new.single_line_change_from(old) {
        return Some(change);
    }
    let mut difference = old.bytes().zip(new.bytes()).position(|(left, right)| left != right).unwrap_or_else(|| old.len().min(new.len()));
    if old == new {
        return None;
    }
    while difference > 0 && (!old.is_char_boundary(difference) || !new.is_char_boundary(difference)) {
        difference -= 1;
    }
    let old_start = old.get(..difference)?.rfind('\n').map_or(0, |offset| offset + 1);
    let new_start = new.get(..difference)?.rfind('\n').map_or(0, |offset| offset + 1);
    let old_end = old[difference..].find('\n').map_or(old.len(), |offset| difference + offset);
    let new_end = new[difference..].find('\n').map_or(new.len(), |offset| difference + offset);
    if old.get(..old_start) != new.get(..new_start) || old.get(old_end..) != new.get(new_end..) {
        return None;
    }
    Some(FrameTextChange { line: old.line_of_byte(old_start), old_start, old_end, new_start, new_end })
}

fn retain_decorations(decorations: &[DecorationSpan], shared: Option<&SharedDecorations>) -> SharedDecorations {
    shared.map_or_else(|| Arc::new(decorations.to_vec()), Arc::clone)
}

fn shared_decoration_layers_equal(left: &[SharedDecorations], right: &[SharedDecorations]) -> bool {
    const SMALL_DYNAMIC_LAYER_LIMIT: usize = 128;
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| Arc::ptr_eq(left, right) || (left.len() <= SMALL_DYNAMIC_LAYER_LIMIT && right.len() == left.len() && left == right))
}

fn merge_shared_decoration_layers(layers: &[SharedDecorations]) -> Vec<DecorationSpan> {
    let capacity = layers.iter().map(|layer| layer.len()).sum();
    let mut merged = Vec::with_capacity(capacity);
    for layer in layers {
        merged.extend(layer.iter().cloned());
    }
    merged.sort_by_key(|span| (span.range.start, std::cmp::Reverse(span.range.end)));
    merged.dedup();
    merged
}

fn visual_line_rows_bounded(text: &str, width: usize, tab_width: usize, limit: usize) -> usize {
    let mut row = 0;
    let mut column = 0;
    for grapheme in text.graphemes(true) {
        advance_visual_grapheme(&mut row, &mut column, width, tab_width, grapheme);
        if row >= limit {
            return limit.saturating_add(1);
        }
    }
    row.saturating_add(1)
}

fn cursor_visual_position(text: &str, start_byte: usize, cursor_byte: usize, width: usize, height: usize, tab_width: usize) -> (usize, usize) {
    if cursor_byte <= start_byte {
        return (0, 0);
    }
    let visible = text.get(start_byte..cursor_byte.min(text.len())).unwrap_or_default();
    let mut row = 0_usize;
    let mut column = 0_usize;
    for grapheme in visible.graphemes(true) {
        advance_visual_grapheme(&mut row, &mut column, width, tab_width, grapheme);
        if row >= height {
            break;
        }
    }
    (column.min(width.saturating_sub(1)), row.min(height.saturating_sub(1)))
}

fn advance_visual_grapheme(row: &mut usize, column: &mut usize, width: usize, tab_width: usize, grapheme: &str) {
    if grapheme == "\n" {
        *row = row.saturating_add(1);
        *column = 0;
        return;
    }
    if grapheme == "\t" {
        for _ in 0..tab_width.max(1) - (*column % tab_width.max(1)) {
            advance_visual_cell(row, column, width, 1);
        }
    } else if grapheme.chars().all(|character| !character.is_control()) {
        advance_visual_cell(row, column, width, UnicodeWidthStr::width(grapheme).max(1));
    } else {
        for escaped in escape_grapheme(grapheme) {
            advance_visual_cell(row, column, width, UnicodeWidthStr::width(escaped.as_str()).max(1));
        }
    }
}

fn advance_visual_cell(row: &mut usize, column: &mut usize, width: usize, cell_width: usize) {
    if column.saturating_add(cell_width) > width && *column > 0 {
        *row = row.saturating_add(1);
        *column = 0;
    }
    *column = column.saturating_add(cell_width);
}

#[must_use]
#[cfg(any(test, feature = "test-support"))]
pub fn diff(previous: Option<&DesiredGrid>, desired: &DesiredGrid) -> TerminalUpdate {
    let mut update = TerminalUpdate::default();
    diff_into(previous, desired, &mut update);
    update
}

/// Reuses caller-owned row storage for successive terminal frames.
pub fn diff_into(previous: Option<&DesiredGrid>, desired: &DesiredGrid, update: &mut TerminalUpdate) {
    update.clear = false;
    update.rows.clear();
    if update.rows.capacity() < desired.height {
        update.rows.reserve(desired.height - update.rows.capacity());
    }
    let full_refresh = previous.is_none_or(|old| old.width != desired.width || old.height != desired.height);
    let raster_rows = desired.raster_overlay.as_ref().map_or(0, |overlay| overlay.rows.min(desired.height));
    if full_refresh {
        update.clear = true;
        let present_rows = desired.rows.len().min(desired.height);
        for (row_index, row) in desired.rows.iter().take(present_rows).enumerate().skip(raster_rows) {
            update.rows.push((row_index, Arc::clone(row)));
        }
        for row_index in present_rows.max(raster_rows)..desired.height {
            update.rows.push((row_index, Arc::default()));
        }
        // A clear invalidates the terminal's raster state along with its cell
        // grid, so the desired overlay must always be submitted again. Besides
        // being the correct resize behavior, this avoids comparing overlays on
        // the full-refresh hot path.
        update.raster_overlay = Some(desired.raster_overlay.clone());
    } else if let Some(previous) = previous {
        for row_index in 0..desired.height {
            let row = desired.rows.get(row_index).cloned().unwrap_or_default();
            if previous.rows.get(row_index) != Some(&row) {
                update.rows.push((row_index, row));
            }
        }
        let previous_overlay = previous.raster_overlay.as_ref();
        update.raster_overlay = (previous_overlay != desired.raster_overlay.as_ref()).then(|| desired.raster_overlay.clone());
    }
    update.cursor = (desired.cursor.0.min(desired.width.saturating_sub(1)), desired.cursor.1.min(desired.height.saturating_sub(1)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_model_tracks_buffers_splits_and_tabs() {
        let mut model = ClientViewModel::initial();
        let second = model.add_buffer();
        model.set_active_buffer(second);
        assert_eq!(model.active_buffer(), second);

        let first_window = model.active_window().id;
        let split = model.split_active(SplitAxis::Vertical).expect("split");
        assert_ne!(split, first_window);
        assert_eq!(model.window_count(), 2);
        model.focus_window(WindowDirection::Left).expect("focus left");
        assert_eq!(model.active_window().id, first_window);
        let lower = model.split_active(SplitAxis::Horizontal).expect("lower split");
        model.focus_window(WindowDirection::Up).expect("focus up");
        assert_eq!(model.active_window().id, first_window);
        model.focus_window(WindowDirection::Down).expect("focus down");
        assert_eq!(model.active_window().id, lower);
        model.close_active_window().expect("close lower split");
        model.focus_window(WindowDirection::Right).expect("focus right");
        assert_eq!(model.active_window().id, split);
        model.close_active_window().expect("close split");
        assert_eq!(model.active_window().id, first_window);

        let first_tab = model.active_tab;
        model.new_tab(second);
        assert_ne!(model.active_tab, first_tab);
        model.cycle_tab(-1);
        assert_eq!(model.active_tab, first_tab);
        model.cycle_tab(1);
        model.close_active_tab().expect("close tab");
        assert_eq!(model.active_tab, first_tab);
    }

    #[test]
    fn workspace_grid_renders_split_panes_and_active_cursor() {
        let mut model = ClientViewModel::initial();
        let first = model.active_buffer();
        let second = model.add_buffer();
        model.split_active(SplitAxis::Vertical).expect("split");
        model.set_active_buffer(second);
        let grid = ViewportLayout::new(15, 4).desired_workspace_grid(
            &model,
            &[(first, EngineFrame::new("left", 0)), (second, EngineFrame::new("right", 5))],
            "status",
            None,
        );
        let first_row = grid.rows[0].text();
        assert!(first_row.contains("left"));
        assert!(first_row.contains('│'));
        assert!(first_row.contains("right"));
        assert!(grid.cursor.0 > 7);
    }

    #[test]
    fn terminal_sidebar_preserves_harness_cells_and_owns_focused_cursor() {
        let model = ClientViewModel::initial();
        let buffer_id = model.active_buffer();
        let frame = EngineFrame::new("editor contents", 0);
        let mut layout = ViewportLayout::new(80, 10);
        layout.set_terminal_sidebar_visible(true);
        let grid = layout.desired_workspace_grid(&model, &[(buffer_id, frame)], "NORMAL", None);
        let column = layout.terminal_sidebar_column().expect("sidebar geometry");
        let harness_style = CellStyle::rgb(RgbColor::new(240, 10, 20), RgbColor::new(10, 20, 120)).with_bold();
        let harness_rows = vec![CellRow { cells: vec![Cell { grapheme: "界".into(), width: 2, style: harness_style }] }];
        let grid = layout.apply_terminal_sidebar(grid, TerminalSidebar { rows: &harness_rows, cursor: (2, 0), focused: true });
        let mut display_column = 0;
        let harness_cell = grid.rows[0]
            .cells
            .iter()
            .find(|cell| {
                let found = display_column == column + 1;
                display_column += usize::from(cell.width);
                found
            })
            .expect("harness cell");
        assert_eq!(harness_cell.grapheme.as_str(), "界");
        assert_eq!(harness_cell.width, 2);
        assert_eq!(harness_cell.style, harness_style);
        assert_eq!(grid.cursor, (column + 3, 0));
        assert_eq!(grid.rows[0].cells.iter().map(|cell| usize::from(cell.width)).sum::<usize>(), 80);
    }

    #[test]
    fn workspace_hit_testing_matches_gutters_tabs_unicode_wraps_splits_and_status() {
        let mut model = ClientViewModel::initial();
        let first = model.active_buffer();
        let first_window = model.active_window().id;
        let second = model.add_buffer();
        let second_window = model.split_active(SplitAxis::Vertical).expect("split");
        model.set_active_buffer(second);
        let frames = [(first, EngineFrame::new("ab界cdef\n\txy\nlast", 0)), (second, EngineFrame::new("right pane", 0))];
        let mut layout = ViewportLayout::new(21, 7);
        layout.configure_dotfile_profile();

        let wide = layout.hit_test_workspace(&model, &frames, 6, 0, 1).expect("wide grapheme");
        assert_eq!((wide.window_id, wide.buffer_id, wide.byte), (first_window, first, 2));
        let wrapped = layout.hit_test_workspace(&model, &frames, 3, 1, 1).expect("wrapped continuation");
        assert_eq!(wrapped.byte, 8);
        let tab = layout.hit_test_workspace(&model, &frames, 4, 2, 1).expect("tab expansion");
        assert_eq!(tab.byte, 10);
        let trailing = layout.hit_test_workspace(&model, &frames, 9, 2, 1).expect("trailing cell");
        assert_eq!(trailing.byte, 12);
        let other_pane = layout.hit_test_workspace(&model, &frames, 14, 0, 1).expect("second pane");
        assert_eq!((other_pane.window_id, other_pane.buffer_id), (second_window, second));
        assert!(layout.hit_test_workspace(&model, &frames, 10, 0, 1).is_none());
        assert!(layout.hit_test_workspace(&model, &frames, 3, 6, 1).is_none());
        let below_file = layout.hit_test_workspace(&model, &frames, 3, 5, 1).expect("blank viewport row clamps to the last buffer line");
        assert_eq!(below_file.byte, "ab界cdef\n\txy\nlast".len() - 1);

        let tiny = ViewportLayout::new(1, 1);
        assert!(tiny.hit_test_workspace(&model, &frames, 0, 0, 1).is_none());

        model.focus_window_id(first_window).expect("focus by click target");
        assert_eq!(model.active_window().id, first_window);
    }

    #[test]
    fn telescope_overlay_renders_results_selection_preview_and_prompt() {
        let frame = EngineFrame::new("underlying editor", 0);
        let mut layout = ViewportLayout::new(100, 30);
        let grid = layout.desired_editor_grid(&frame, "NORMAL", None);
        let grid = layout.apply_picker_overlay(
            grid,
            &PickerOverlay {
                title: "Find Files".into(),
                prompt: "main".into(),
                rows: vec![
                    MenuOverlayRow { label: "README.md".into(), detail: "".into(), source: None },
                    MenuOverlayRow { label: "src/main.rs".into(), detail: "Rust".into(), source: None },
                ],
                selected: 1,
                preview_title: "src/main.rs".into(),
                preview: "fn main() {\n    println!(\"wren\");\n}".into(),
                preview_highlight_line: Some(1),
                preview_decorations: vec![DecorationSpan::new(
                    0..2,
                    CellStyle::default()
                        .with_foreground(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Mauve)))
                        .with_bold(),
                    100,
                )],
                footer: "↑/↓ select  ⏎ open".into(),
                ..PickerOverlay::default()
            },
        );
        let rendered = grid.text();
        assert!(rendered.contains("Find Files (2)"));
        assert!(rendered.contains("❯ src/main.rs  Rust"));
        assert!(rendered.contains("println!(\"wren\")"));
        assert!(rendered.contains("❯ main"));
        assert_eq!(grid.cursor.1, 25);
    }

    #[test]
    fn picker_overlay_preserves_the_already_themed_editor_background() {
        let mut theme = CatppuccinPalette::for_flavor(CatppuccinFlavor::Latte);
        // Deliberately overlap a customized slot with a Mocha source color.
        // Applying the theme a second time would reinterpret this background
        // as `surface0` and visibly recolor the editor behind the picker.
        assert!(theme.set("base", CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Surface0)));
        let mut layout = ViewportLayout::new(80, 24);
        layout.set_theme(theme);
        let base = layout.desired_editor_grid(&EngineFrame::new("editor background", 0), "NORMAL", None);
        let outside = base.rows[0].cells[0].style;
        let grid = layout.apply_picker_overlay(
            base,
            &PickerOverlay { title: "Find Files".into(), preview_title: "No preview".into(), footer: "Esc close".into(), ..PickerOverlay::default() },
        );

        assert_eq!(grid.rows[0].cells[0].style, outside);
        assert_eq!(grid.rows[0].cells[0].style.background, Some(CellColor::Rgb(theme.color(CatppuccinColor::Base))));
    }

    #[test]
    fn telescope_preview_wraps_long_decorated_lines_inside_its_border() {
        let mut layout = ViewportLayout::new(100, 30);
        let grid = layout.desired_editor_grid(&EngineFrame::new("underlying editor", 0), "NORMAL", None);
        let preview = "x".repeat(140);
        let grid = layout.apply_picker_overlay(
            grid,
            &PickerOverlay {
                title: "Find Files".into(),
                rows: vec![MenuOverlayRow { label: "src/main.rs".into(), detail: "".into(), source: None }],
                preview_title: "src/main.rs".into(),
                preview: preview.clone().into(),
                preview_highlight_line: Some(0),
                preview_decorations: vec![DecorationSpan::new(
                    0..preview.len(),
                    CellStyle::default()
                        .with_foreground(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Green)))
                        .with_bold(),
                    100,
                )],
                footer: "enter open".into(),
                ..PickerOverlay::default()
            },
        );

        // At this terminal size the floating picker's right border is column
        // 97 and its preview begins at 41. All 140 cells wrap inside it.
        for row in 5..25 {
            assert_eq!(grid.rows[row].cells[97].grapheme.as_str(), "│");
        }
        let rendered_x = grid.rows.iter().flat_map(|row| row.cells.iter()).filter(|cell| cell.grapheme.as_str() == "x").count();
        assert_eq!(rendered_x, preview.len());
        assert!(grid.rows.iter().all(|row| { row.cells.iter().map(|cell| usize::from(cell.width)).sum::<usize>() <= 100 }));
    }

    #[test]
    fn telescope_preview_uses_a_lighter_background_only_for_the_reference() {
        let mut layout = ViewportLayout::new(100, 30);
        let base = layout.desired_editor_grid(&EngineFrame::new("underlying editor", 0), "NORMAL", None);
        let picker = PickerOverlay {
            title: "Jumplist".into(),
            rows: vec![MenuOverlayRow { label: "src/main.rs:1:7".into(), detail: "language-server location".into(), source: None }],
            preview_title: "src/main.rs".into(),
            preview: "alpha target omega".into(),
            preview_decorations: vec![
                DecorationSpan::new(
                    0..5,
                    CellStyle::default()
                        .with_foreground(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Green)))
                        .without_background(),
                    100,
                ),
                DecorationSpan::new(
                    6..12,
                    CellStyle::default()
                        .without_foreground()
                        .with_background(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Surface0))),
                    u32::MAX,
                ),
            ],
            footer: "enter open".into(),
            ..PickerOverlay::default()
        };
        let geometry = layout.picker_geometry();
        let preview_column = geometry.column + 2 + geometry.result_width;

        let grid = layout.apply_picker_overlay(base, &picker);
        let row = &grid.rows[geometry.content_start];

        assert_eq!(
            row.cells[preview_column].style.foreground,
            Some(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Green)))
        );
        assert_eq!(
            row.cells[preview_column].style.background,
            Some(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Mantle)))
        );
        for cell in &row.cells[preview_column + 6..preview_column + 12] {
            assert_eq!(cell.style.background, Some(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Surface0))));
        }
        assert_eq!(
            row.cells[preview_column + 12].style.background,
            Some(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Mantle)))
        );
    }

    #[test]
    fn dap_ui_overlay_renders_default_panels_without_replacing_the_editor() {
        let mut layout = ViewportLayout::new(100, 30);
        let grid = layout.desired_editor_grid(&EngineFrame::new("fn main() {}", 0), "NORMAL", None);
        let grid = layout.apply_debug_overlay(
            grid,
            &DebugOverlay {
                panels: [
                    DebugPanel { title: "Scopes", text: "▸ Locals".into() },
                    DebugPanel { title: "Breakpoints", text: "● main.rs:1".into() },
                    DebugPanel { title: "Stacks", text: "▾ current thread".into() },
                    DebugPanel { title: "Watches", text: "watch value".into() },
                    DebugPanel { title: "REPL", text: "(lldb)".into() },
                    DebugPanel { title: "Console", text: "stopped".into() },
                ],
            },
        );
        let rendered = grid.text();
        for title in ["Scopes", "Breakpoints", "Stacks", "Watches", "REPL", "Console"] {
            assert!(rendered.contains(title), "missing {title}: {rendered}");
        }
        assert!(rendered.contains("fn main()"));
    }

    #[test]
    fn catppuccin_flavors_are_exact_truecolor_and_overridable() {
        assert_eq!(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Base), RgbColor::new(0x1e, 0x1e, 0x2e));
        assert_eq!(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Text), RgbColor::new(0xcd, 0xd6, 0xf4));
        assert_eq!(CatppuccinPalette::for_flavor(CatppuccinFlavor::Latte).color(CatppuccinColor::Base), RgbColor::new(0xef, 0xf1, 0xf5));
        let mut palette = CatppuccinPalette::for_flavor(CatppuccinFlavor::Macchiato);
        assert!(palette.set("mauve", RgbColor::new(1, 2, 3)));
        assert_eq!(palette.color(CatppuccinColor::Mauve), RgbColor::new(1, 2, 3));
        assert!(!palette.set("not-a-slot", RgbColor::new(1, 2, 3)));

        let mut layout = ViewportLayout::new(20, 2);
        layout.set_theme(CatppuccinPalette::for_flavor(CatppuccinFlavor::Latte));
        let grid = layout.desired_editor_grid(&EngineFrame::new("x", 0), "", None);
        assert_eq!(
            grid.rows[0].cells[0].style,
            CellStyle::rgb(
                CatppuccinPalette::for_flavor(CatppuccinFlavor::Latte).color(CatppuccinColor::Text),
                CatppuccinPalette::for_flavor(CatppuccinFlavor::Latte).color(CatppuccinColor::Base)
            )
        );
    }

    #[test]
    fn every_terminal_row_erases_with_the_active_theme_background() {
        let mut layout = ViewportLayout::new(120, 40);
        layout.set_theme(CatppuccinPalette::for_flavor(CatppuccinFlavor::Macchiato));
        let grid = layout.desired_editor_grid(&EngineFrame::new("one short line", 0), "NORMAL", None);
        assert_eq!(grid.rows.len(), 40);
        assert!(grid.rows.iter().all(|row| !row.cells.is_empty()));
        let patches = diff(None, &grid);
        let clears = patches.rows.iter().filter_map(|(_, cells)| cells.cells.last().map(|cell| &cell.style)).collect::<Vec<_>>();
        assert_eq!(clears.len(), 40);
        assert!(clears.iter().all(|style| {
            style.background == Some(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Macchiato).color(CatppuccinColor::Base)))
        }));
    }

    #[test]
    fn raster_overlay_is_added_and_removed_by_the_grid_diff() {
        let mut with_overlay = ViewportLayout::new(2, 2).desired_grid(&EngineFrame::new("", 0));
        with_overlay.raster_overlay = Some(Arc::new(RasterOverlay {
            frame_id: with_overlay.epoch,
            width: 2,
            height: 2,
            columns: 2,
            rows: 1,
            background: RgbColor::new(0, 0, 0),
            quads: Arc::new(Vec::new()),
        }));
        assert!(matches!(diff(None, &with_overlay).raster_overlay, Some(Some(_))));

        let mut without_overlay = with_overlay.clone();
        without_overlay.raster_overlay = None;
        assert_eq!(diff(Some(&with_overlay), &without_overlay).raster_overlay, Some(None));
    }

    #[test]
    fn wraps_graphemes_without_splitting_them() {
        let frame = EngineFrame::new("ab界c", "ab界".len());
        let grid = ViewportLayout::new(3, 3).desired_grid(&frame);
        assert_eq!(grid.rows[1].cells[0].grapheme.as_str(), "界");
        assert_eq!(grid.cursor, (2, 1));
    }

    #[test]
    fn dotfile_profile_renders_relative_numbers_two_column_tabs_and_scrolloff() {
        let frame = EngineFrame::new("one\n\ttwo\nthree\nfour\nfive\nsix\nseven\n", "one\n\tt".len());
        let mut layout = ViewportLayout::new(20, 5);
        layout.configure_dotfile_profile();
        layout.ensure_cursor_visible(&frame, 0);
        let grid = layout.desired_grid(&frame);
        let rows = grid.rows.iter().map(|row| row.text()).collect::<Vec<_>>();
        assert!(rows[0].starts_with(" 1 one"));
        assert!(rows[1].starts_with(" 2   two"));
        assert_eq!(grid.cursor.0, 6);

        let mut wide = ViewportLayout::new(90, 2);
        wide.configure_dotfile_profile();
        let wide_grid = wide.desired_grid(&EngineFrame::new("short\n", 0));
        let color_column = &wide_grid.rows[0].cells[82];
        assert_eq!(color_column.grapheme.as_str(), " ");
        assert_eq!(color_column.style.background, Some(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Mantle))));
        assert!(!color_column.style.reverse());
    }

    #[test]
    fn text_past_the_color_limit_keeps_the_same_edge_on_wrapped_rows() {
        let mut layout = ViewportLayout::new(90, 3);
        layout.configure_dotfile_profile();
        let grid = layout.desired_grid(&EngineFrame::new("x".repeat(180), 0));

        for row in grid.rows.iter().take(2) {
            assert_eq!(row.cells[81].grapheme.as_str(), "x");
            assert_eq!(
                row.cells[81].style.background,
                Some(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Base)))
            );
            assert_eq!(row.cells[82].grapheme.as_str(), "x");
            assert_eq!(
                row.cells[82].style.background,
                Some(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Mantle)))
            );
            assert_eq!(row.cells[83].grapheme.as_str(), "x");
            assert_eq!(
                row.cells[83].style.background,
                Some(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Mantle)))
            );
        }
    }

    #[test]
    fn text_popup_crossing_color_limit_preserves_the_underlying_tail_background() {
        let mut layout = ViewportLayout::new(120, 8);
        layout.configure_dotfile_profile();
        let source = (0..8).map(|_| format!("left{}right\n", " ".repeat(90))).collect::<String>();
        let base = layout.desired_grid(&EngineFrame::new(source, 4));
        let popup = TextPopup::new("hover", "a".repeat(80));

        let grid = layout.apply_text_popup(base, &popup);
        let interior = &grid.rows[2];

        assert_eq!(
            interior.cells[82].style.background,
            Some(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Mantle))),
            "popup must repaint the color-column cell with its own surface"
        );
        assert_eq!(
            interior.cells[100].style.background,
            Some(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Mantle))),
            "the terminal tail beyond the popup must retain the editor overflow background"
        );
    }

    #[test]
    fn text_popup_uses_empty_right_margin_instead_of_following_the_cursor() {
        let source = "fn one() {}\nfn two() {}\nfn three() {}\nfn four() {}\n";
        let popup = TextPopup::new("hover", "documentation");
        let mut layout = ViewportLayout::new(100, 12);

        let first = layout.desired_grid(&EngineFrame::new(source, 0));
        let first_grid = layout.apply_text_popup(first.clone(), &popup);
        let first_placement = text_popup_placement(&first, 15, 3);
        let second = layout.desired_grid(&EngineFrame::new(source, source.find("four").expect("four")));
        let second_grid = layout.apply_text_popup(second.clone(), &popup);
        let second_placement = text_popup_placement(&second, 15, 3);

        assert_eq!(first_placement, PopupPlacement { column: 84, row: 0 });
        assert_eq!(second_placement, first_placement);
        assert_eq!(first_grid.rows[0].cells[84].grapheme.as_str(), "╭");
        assert_eq!(second_grid.rows[0].cells[84].grapheme.as_str(), "╭");
        assert_eq!(first_grid.rows[0].cells[98].grapheme.as_str(), "╮");
        assert_ne!(first_grid.rows[0].cells[99].grapheme.as_str(), "╮");
    }

    #[test]
    fn text_popup_keeps_a_margin_after_its_right_border() {
        let mut layout = ViewportLayout::new(40, 8);
        let base = layout.desired_grid(&EngineFrame::new("short\n", 0));
        let popup = TextPopup::new("hover", "x".repeat(80));
        let placement = text_popup_placement(&base, 38, 5);

        let grid = layout.apply_text_popup(base, &popup);

        assert_eq!(placement.column, 1);
        assert_eq!(grid.rows[placement.row].cells[1].grapheme.as_str(), "╭");
        assert_eq!(grid.rows[placement.row].cells[38].grapheme.as_str(), "╮");
        assert_ne!(grid.rows[placement.row].cells[39].grapheme.as_str(), "╮");
    }

    #[test]
    fn focused_text_popup_places_the_terminal_cursor_inside_its_content() {
        let mut layout = ViewportLayout::new(40, 8);
        let base = layout.desired_grid(&EngineFrame::new("buffer\n", 0));
        let popup = TextPopup::new("hover", "one\ntwo\nthree").with_cursor((1, 1));
        let placement = text_popup_placement(&base, 7, 5);

        let grid = layout.apply_text_popup(base, &popup);

        assert_eq!(grid.cursor, (placement.column + 2, placement.row + 2));
    }

    #[test]
    fn text_popup_finds_empty_right_edge_below_longer_lines() {
        let mut layout = ViewportLayout::new(40, 8);
        let grid = layout.desired_grid(&EngineFrame::new("                              right\n                              edge\n", 30));

        assert_eq!(text_popup_placement(&grid, 12, 4), PopupPlacement { column: 27, row: 2 });
    }

    #[test]
    fn one_cell_terminal_renders_status_without_indexing_past_the_grid() {
        let mut layout = ViewportLayout::new(1, 1);
        layout.configure_dotfile_profile();
        let model = ClientViewModel::initial();
        let frame = EngineFrame::new("x", 0);
        let grid = layout.desired_workspace_grid(&model, &[(BufferId::new(1), frame)], "NORMAL", None);
        assert_eq!(grid.rows.len(), 1);
    }

    #[test]
    fn controls_are_escaped_before_terminal_patches() {
        let frame = EngineFrame::new("a\u{1b}\u{0}b", 0);
        let grid = ViewportLayout::new(20, 2).desired_grid(&frame);
        let visible = grid.rows[0].text();
        assert_eq!(visible, "a^[^@b");
        assert!(!visible.contains('\u{1b}'));
    }

    #[test]
    fn decorations_follow_byte_ranges_through_unicode_tabs_and_splits() {
        let mut model = ClientViewModel::initial();
        let first = model.active_buffer();
        let second = model.add_buffer();
        model.split_active(SplitAxis::Vertical).expect("split");
        model.set_active_buffer(second);
        let keyword = CellStyle::default().with_foreground(CellColor::Palette(12)).with_bold();
        let grid = ViewportLayout::new(20, 3).desired_workspace_grid_with_decorations(
            &model,
            &[(first, EngineFrame::new("界\tfn", 0)), (second, EngineFrame::new("let value", 0))],
            &[(first, vec![DecorationSpan::new("界\t".len().."界\tfn".len(), keyword, 100)]), (second, vec![DecorationSpan::new(0..3, keyword, 100)])],
            "",
            None,
        );
        let styled: String =
            grid.rows.iter().flat_map(|row| row.cells.iter()).filter(|cell| cell.style == keyword).map(|cell| cell.grapheme.as_str()).collect();
        assert_eq!(styled, "fnlet");
    }

    #[test]
    fn signcolumn_number_decorations_do_not_overwrite_syntax_tokens() {
        let syntax = CellStyle::default()
            .with_foreground(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Mauve)))
            .with_bold();
        let git_sign = CellStyle::default()
            .with_foreground(CellColor::Rgb(CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Green)))
            .with_bold();
        let mut layout = ViewportLayout::new(20, 2);
        layout.configure_dotfile_profile();
        let grid = layout.desired_editor_grid_with_line_decorations(
            &EngineFrame::new("fn main() {}\n", 0),
            "",
            None,
            &[DecorationSpan::new(0..2, syntax, 100)],
            &[LineDecoration { line: 0, style: git_sign }],
        );

        assert!(grid.rows[0].cells[..3].iter().all(|cell| cell.style.foreground == git_sign.foreground));
        assert_eq!(grid.rows[0].cells[3].grapheme.as_str(), "f");
        assert_eq!(grid.rows[0].cells[3].style.foreground, syntax.foreground);
        assert_eq!(grid.rows[0].cells[4].grapheme.as_str(), "n");
        assert_eq!(grid.rows[0].cells[4].style.foreground, syntax.foreground);
    }

    #[test]
    fn cursor_scrolls_into_view_and_prompt_owns_last_row() {
        let frame = EngineFrame::new("0\n1\n2\n3\n4\n", "0\n1\n2\n3\n".len());
        let mut layout = ViewportLayout::new(10, 3);
        layout.ensure_cursor_visible(&frame, 1);
        assert_eq!(layout.top_line, 3);
        let grid = layout.desired_editor_grid(&frame, "", Some(":w"));
        assert_eq!(grid.cursor, (2, 2));
    }

    #[test]
    fn shrinking_rows_replaces_the_terminal_row() {
        let old = ViewportLayout::new(20, 2).desired_grid(&EngineFrame::new("long line", 0));
        let new = ViewportLayout::new(20, 2).desired_grid(&EngineFrame::new("x", 0));
        assert!(!diff(Some(&old), &new).rows.is_empty());
    }

    #[test]
    fn unchanged_rows_are_arc_reused_while_frame_epochs_advance() {
        let frame = EngineFrame::new("same\nrows", 0);
        let mut layout = ViewportLayout::new(20, 2);
        let first = layout.desired_grid(&frame);
        let second = layout.desired_grid(&EngineFrame::new(frame.text, 5));
        assert!(second.epoch > first.epoch);
        assert!(Arc::ptr_eq(&first.rows[0], &second.rows[0]));
        assert!(Arc::ptr_eq(&first.rows[1], &second.rows[1]));
        assert_eq!(second.cursor, (0, 1));
    }

    #[test]
    fn unicode_line_edits_only_rebuild_the_changed_visual_rows() {
        let mut layout = ViewportLayout::new(30, 3);
        let first = layout.desired_grid(&EngineFrame::new("界 alpha\nβeta\ngamma", 0));
        let second = layout.desired_grid(&EngineFrame::new("界λ alpha\nβeta\ngamma", "界λ".len()));

        assert!(!Arc::ptr_eq(&first.rows[0], &second.rows[0]));
        assert!(Arc::ptr_eq(&first.rows[1], &second.rows[1]));
        assert!(Arc::ptr_eq(&first.rows[2], &second.rows[2]));
        assert_eq!(second.cursor, (3, 0));
        assert_eq!(second.rows[0].text(), "界λ alpha");
    }

    #[test]
    fn one_line_viewport_scroll_reuses_visible_unicode_rows() {
        let frame = EngineFrame::new("αlpha\nβeta\nγamma\nδelta", 0);
        let mut layout = ViewportLayout::new(30, 3);
        let first = layout.desired_grid(&frame);
        layout.top_line = 1;
        let second = layout.desired_grid(&frame);

        assert!(Arc::ptr_eq(&first.rows[1], &second.rows[0]));
        assert!(Arc::ptr_eq(&first.rows[2], &second.rows[1]));
        assert_eq!(second.rows[2].text(), "δelta");
    }

    #[test]
    fn half_page_viewport_scroll_reuses_the_overlapping_rows_in_both_directions() {
        let text = (0..20).map(|line| format!("line {line:02}\n")).collect::<String>();
        let frame = EngineFrame::new(text, 0);
        let mut layout = ViewportLayout::new(30, 8);
        let first = layout.desired_grid(&frame);
        layout.top_line = 4;
        let down = layout.desired_grid(&frame);

        for offset in 0..4 {
            assert!(Arc::ptr_eq(&first.rows[offset + 4], &down.rows[offset]));
        }

        layout.top_line = 2;
        let up = layout.desired_grid(&frame);
        for offset in 0..6 {
            assert!(Arc::ptr_eq(&down.rows[offset], &up.rows[offset + 2]));
        }
        assert_eq!(up.rows[0].text(), "line 02");
        assert_eq!(up.rows[1].text(), "line 03");
    }

    #[test]
    fn distant_viewport_jump_reuses_a_recent_view_without_rerendering_rows() {
        let text = (0..100).map(|line| format!("line {line:02}\n")).collect::<String>();
        let frame = EngineFrame::new(text, 0);
        let mut layout = ViewportLayout::new(30, 8);
        let first = layout.desired_grid(&frame);

        layout.top_line = 80;
        let bottom = layout.desired_grid(&frame);
        assert!(!Arc::ptr_eq(&first.rows[0], &bottom.rows[0]));

        layout.top_line = 0;
        let returned = layout.desired_grid(&frame);
        for (original, reused) in first.rows.iter().zip(&returned.rows) {
            assert!(Arc::ptr_eq(original, reused));
        }
    }

    #[test]
    fn dotfile_relative_numbers_reuse_recent_rows_across_back_and_forth_scrolls() {
        let text = (0..20).map(|line| format!("line {line:02}\n")).collect::<String>();
        let text = FrameText::from(text);
        let mut layout = ViewportLayout::new(30, 8);
        layout.configure_dotfile_profile();
        let row_text = CellRow::text;

        let first = layout.desired_editor_grid(&EngineFrame::new(text.clone(), 0), "", None);
        layout.top_line = 4;
        let down = layout.desired_editor_grid(&EngineFrame::new(text.clone(), text.byte_of_line(4)), "", None);
        assert!((0..4).all(|line| layout.has_cached_logical_line(line)));
        assert!(row_text(&down.rows[0]).starts_with(" 5 line 04"));

        layout.top_line = 0;
        let up = layout.desired_editor_grid(&EngineFrame::new(text, 0), "", None);
        assert!(row_text(&up.rows[0]).starts_with(" 1 line 00"));
        assert_eq!(row_text(&up.rows[4]), row_text(&first.rows[4]));
        for (original, reused) in first.rows.iter().zip(&up.rows) {
            assert!(Arc::ptr_eq(original, reused));
        }
    }

    #[test]
    fn rich_status_overlay_builds_once_and_reuses_the_exact_row() {
        let mut layout = ViewportLayout::new(20, 3);
        let status = StatusOverlay {
            left: vec![StatusSegment { text: " LEFT ".into(), style: CellStyle::default().with_bold() }],
            right: vec![StatusSegment { text: " R ".into(), style: CellStyle::default().with_reverse() }],
        };
        let base = || DesiredGrid {
            epoch: 0,
            width: 20,
            height: 3,
            rows: (0..3).map(|_| Arc::new(CellRow::default())).collect(),
            cursor: (0, 0),
            raster_overlay: None,
        };
        let first = layout.apply_status_overlay(base(), &status);
        let second = layout.apply_status_overlay(base(), &status);
        assert!(Arc::ptr_eq(&first.rows[2], &second.rows[2]));
        let rendered = first.rows[2].text();
        assert_eq!(rendered, " LEFT             R ");
    }

    #[test]
    fn production_workspace_retains_unchanged_pane_rows() {
        let model = ClientViewModel::initial();
        let buffer_id = model.active_buffer();
        let window_id = model.active_window().id;
        let mut layout = ViewportLayout::new(30, 4);
        let first = layout.desired_workspace_grid(&model, &[(buffer_id, EngineFrame::new("界 alpha\nβeta\ngamma", 0))], "NORMAL", None);
        let cached_second_row = Arc::clone(&layout.pane_layouts[&window_id].cached_rows[1]);
        let second = layout.desired_workspace_grid(&model, &[(buffer_id, EngineFrame::new("界λ alpha\nβeta\ngamma", "界λ".len()))], "NORMAL", None);

        assert!(Arc::ptr_eq(&cached_second_row, &layout.pane_layouts[&window_id].cached_rows[1]));
        assert!(Arc::ptr_eq(&first.rows[1], &second.rows[1]));
        assert_eq!(second.cursor, (3, 0));
    }

    #[test]
    fn offscreen_line_edits_do_not_replace_visible_rows() {
        let mut layout = ViewportLayout::new(30, 2);
        layout.top_line = 1;
        let first = layout.desired_grid(&EngineFrame::new("alpha\nbeta\ngamma", "alpha\n".len()));
        let second = layout.desired_grid(&EngineFrame::new("alphax\nbeta\ngamma", "alphax\n".len()));

        assert!(Arc::ptr_eq(&first.rows[0], &second.rows[0]));
        assert!(Arc::ptr_eq(&first.rows[1], &second.rows[1]));
        assert_eq!(second.rows[0].text(), "beta");
    }

    #[test]
    fn ordered_decoration_sweep_matches_full_overlap_resolution() {
        let decorations = vec![
            DecorationSpan::new(4..9, CellStyle::default().with_foreground(CellColor::Palette(2)), 20),
            DecorationSpan::new(0..12, CellStyle::default().with_foreground(CellColor::Palette(1)).with_bold(), 10),
            DecorationSpan::new(7..11, CellStyle::default().with_foreground(CellColor::Palette(3)).with_underline(), 20),
            DecorationSpan::new(2..5, CellStyle::default().with_background(CellColor::Palette(4)), 30),
        ];
        let mut resolver = DecorationResolver::new(&decorations);
        for start in 0..14 {
            let range = start..start + 1;
            assert_eq!(resolver.style(range.clone()), decoration_style(&decorations, range));
        }
        let mut resolver = DecorationResolver::new_at(&decorations, 7);
        for start in 7..14 {
            let range = start..start + 1;
            assert_eq!(resolver.style(range.clone()), decoration_style(&decorations, range));
        }
    }

    #[test]
    fn shared_decoration_composition_reuses_equal_layers_by_value() {
        let span = |range, priority| DecorationSpan::new(range, CellStyle::default(), priority);
        let buffer_id = BufferId::new(1);
        let mut layout = ViewportLayout::new(80, 24);
        let first_layers = vec![(buffer_id, vec![Arc::new(vec![span(0..2, 1)]), Arc::new(vec![span(1..3, 2)])])];
        let first = layout.compose_shared_decoration_layers(&first_layers);
        assert_eq!(first[0].1.len(), 2);

        let equal_layers = vec![(buffer_id, vec![Arc::new(vec![span(0..2, 1)]), Arc::new(vec![span(1..3, 2)])])];
        let equal = layout.compose_shared_decoration_layers(&equal_layers);
        assert!(Arc::ptr_eq(&first[0].1, &equal[0].1));

        let changed_layers = vec![(buffer_id, vec![Arc::new(vec![span(0..2, 1)]), Arc::new(vec![span(2..4, 2)])])];
        let changed = layout.compose_shared_decoration_layers(&changed_layers);
        assert!(!Arc::ptr_eq(&first[0].1, &changed[0].1));
        assert_eq!(changed[0].1[1].range, 2..4);

        let only = Arc::new(vec![span(4..5, 3)]);
        let single = layout.compose_shared_decoration_layers(&[(buffer_id, vec![Arc::clone(&only)])]);
        assert!(Arc::ptr_eq(&only, &single[0].1));
    }
}
