#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use wren_engine::{EngineFrame, FrameText};
use wren_types::{
    Bias, BufferId, DocumentId, DocumentRevision, Edit, FloatingSurfaceId, TabId, Transaction,
    ViewId, WindowId,
};

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
pub enum SplitTree {
    Window(WindowId),
    Split {
        axis: SplitAxis,
        ratio_milli: u16,
        first: Box<SplitTree>,
        second: Box<SplitTree>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferView {
    pub id: BufferId,
    pub document_id: DocumentId,
    pub name: Box<str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowView {
    pub id: WindowId,
    pub view_id: ViewId,
    pub buffer_id: BufferId,
    pub cursor_byte: usize,
    pub top_line: usize,
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
pub struct FloatingSurface {
    pub id: FloatingSurfaceId,
    pub owner: WindowId,
    pub kind: Box<str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extmark {
    pub document_id: DocumentId,
    pub range: Range<usize>,
    pub kind: Box<str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptSurface {
    pub id: FloatingSurfaceId,
    pub owner: WindowId,
    pub prefix: char,
    pub input: Box<str>,
    pub cursor_byte: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CommandLineState {
    pub prompt: Option<PromptSurface>,
    pub history_index: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageEntry {
    pub sequence: u64,
    pub severity: MessageSeverity,
    pub text: Box<str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MessageLog {
    pub entries: Vec<MessageEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSurface {
    pub buffer_id: BufferId,
    pub rows: u16,
    pub columns: u16,
    pub cursor: (u16, u16),
    pub contents: Box<str>,
    pub running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientViewModel {
    pub buffers: Vec<BufferView>,
    pub windows: Vec<WindowView>,
    pub tabs: Vec<TabView>,
    pub active_tab: TabId,
    pub floating: Vec<FloatingSurface>,
    pub extmarks: Vec<Extmark>,
    pub command_line: CommandLineState,
    pub messages: MessageLog,
    pub terminals: Vec<TerminalSurface>,
    next_id: u64,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ViewModelError {
    #[error("unknown buffer {0:?}")]
    UnknownBuffer(BufferId),
    #[error("unknown window {0:?}")]
    UnknownWindow(WindowId),
    #[error("unknown tab {0:?}")]
    UnknownTab(TabId),
    #[error("cannot close the final window")]
    FinalWindow,
    #[error("cannot close the final tab")]
    FinalTab,
}

impl ClientViewModel {
    #[must_use]
    pub fn new(document_id: DocumentId, name: impl Into<Box<str>>) -> Self {
        let buffer_id = BufferId::new(1);
        let window_id = WindowId::new(2);
        let tab_id = TabId::new(4);
        Self {
            buffers: vec![BufferView {
                id: buffer_id,
                document_id,
                name: name.into(),
            }],
            windows: vec![WindowView {
                id: window_id,
                view_id: ViewId::new(3),
                buffer_id,
                cursor_byte: 0,
                top_line: 0,
            }],
            tabs: vec![TabView {
                id: tab_id,
                root: SplitTree::Window(window_id),
                active_window: window_id,
            }],
            active_tab: tab_id,
            floating: Vec::new(),
            extmarks: Vec::new(),
            command_line: CommandLineState::default(),
            messages: MessageLog::default(),
            terminals: Vec::new(),
            next_id: 5,
        }
    }

    #[must_use]
    pub fn active_tab(&self) -> &TabView {
        // The constructor and all mutators preserve this invariant.
        self.tabs
            .iter()
            .find(|tab| tab.id == self.active_tab)
            .unwrap_or_else(|| &self.tabs[0])
    }

    #[must_use]
    pub fn active_window(&self) -> &WindowView {
        let id = self.active_tab().active_window;
        self.windows
            .iter()
            .find(|window| window.id == id)
            .unwrap_or_else(|| &self.windows[0])
    }

    pub fn active_window_mut(&mut self) -> &mut WindowView {
        let id = self.active_tab().active_window;
        let index = self
            .windows
            .iter()
            .position(|window| window.id == id)
            .unwrap_or(0);
        &mut self.windows[index]
    }

    #[must_use]
    pub fn active_buffer(&self) -> BufferId {
        self.active_window().buffer_id
    }

    pub fn add_buffer(&mut self, document_id: DocumentId, name: impl Into<Box<str>>) -> BufferId {
        let id = BufferId::new(self.allocate());
        self.buffers.push(BufferView {
            id,
            document_id,
            name: name.into(),
        });
        id
    }

    pub fn set_active_buffer(&mut self, buffer_id: BufferId) -> Result<(), ViewModelError> {
        if !self.buffers.iter().any(|buffer| buffer.id == buffer_id) {
            return Err(ViewModelError::UnknownBuffer(buffer_id));
        }
        let window_id = self.active_tab().active_window;
        let window = self
            .windows
            .iter_mut()
            .find(|window| window.id == window_id)
            .ok_or(ViewModelError::UnknownWindow(window_id))?;
        window.buffer_id = buffer_id;
        Ok(())
    }

    pub fn remove_buffer(
        &mut self,
        buffer_id: BufferId,
        replacement: BufferId,
    ) -> Result<(), ViewModelError> {
        if !self.buffers.iter().any(|buffer| buffer.id == replacement) {
            return Err(ViewModelError::UnknownBuffer(replacement));
        }
        if !self.buffers.iter().any(|buffer| buffer.id == buffer_id) {
            return Err(ViewModelError::UnknownBuffer(buffer_id));
        }
        for window in &mut self.windows {
            if window.buffer_id == buffer_id {
                window.buffer_id = replacement;
            }
        }
        self.buffers.retain(|buffer| buffer.id != buffer_id);
        Ok(())
    }

    pub fn split_active(&mut self, axis: SplitAxis) -> Result<WindowId, ViewModelError> {
        let active = self.active_tab().active_window;
        let buffer_id = self.active_window().buffer_id;
        let window_id = WindowId::new(self.allocate());
        let view_id = ViewId::new(self.allocate());
        self.windows.push(WindowView {
            id: window_id,
            view_id,
            buffer_id,
            cursor_byte: 0,
            top_line: 0,
        });
        let tab_id = self.active_tab;
        let tab = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == tab_id)
            .ok_or(ViewModelError::UnknownTab(tab_id))?;
        replace_window_with_split(&mut tab.root, active, window_id, axis)?;
        tab.active_window = window_id;
        Ok(window_id)
    }

    pub fn close_active_window(&mut self) -> Result<(), ViewModelError> {
        let tab_id = self.active_tab;
        let active = self.active_tab().active_window;
        let tab = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == tab_id)
            .ok_or(ViewModelError::UnknownTab(tab_id))?;
        let replacement =
            remove_window(&mut tab.root, active).ok_or(ViewModelError::FinalWindow)?;
        tab.active_window = replacement;
        self.windows.retain(|window| window.id != active);
        Ok(())
    }

    pub fn new_tab(&mut self, buffer_id: BufferId) -> Result<TabId, ViewModelError> {
        if !self.buffers.iter().any(|buffer| buffer.id == buffer_id) {
            return Err(ViewModelError::UnknownBuffer(buffer_id));
        }
        let window_id = WindowId::new(self.allocate());
        let view_id = ViewId::new(self.allocate());
        let tab_id = TabId::new(self.allocate());
        self.windows.push(WindowView {
            id: window_id,
            view_id,
            buffer_id,
            cursor_byte: 0,
            top_line: 0,
        });
        self.tabs.push(TabView {
            id: tab_id,
            root: SplitTree::Window(window_id),
            active_window: window_id,
        });
        self.active_tab = tab_id;
        Ok(tab_id)
    }

    pub fn cycle_tab(&mut self, delta: isize) {
        let current = self
            .tabs
            .iter()
            .position(|tab| tab.id == self.active_tab)
            .unwrap_or(0);
        let len = self.tabs.len();
        let next = current.wrapping_add_signed(delta).rem_euclid(len);
        self.active_tab = self.tabs[next].id;
    }

    pub fn focus_window(&mut self, direction: WindowDirection) -> Result<(), ViewModelError> {
        let tab_index = self
            .tabs
            .iter()
            .position(|tab| tab.id == self.active_tab)
            .ok_or(ViewModelError::UnknownTab(self.active_tab))?;
        let active = self.tabs[tab_index].active_window;
        let mut panes = Vec::new();
        split_rectangles(
            &self.tabs[tab_index].root,
            Rect {
                column: 0,
                row: 0,
                width: 1_000_000,
                height: 1_000_000,
            },
            &mut panes,
        );
        let current = panes
            .iter()
            .find(|(window, _)| *window == active)
            .map(|(_, rectangle)| *rectangle)
            .ok_or(ViewModelError::UnknownWindow(active))?;
        let current_x = current.column.saturating_mul(2) + current.width;
        let current_y = current.row.saturating_mul(2) + current.height;
        let candidate = panes
            .into_iter()
            .filter(|(window, _)| *window != active)
            .filter_map(|(window, rectangle)| {
                let x = rectangle.column.saturating_mul(2) + rectangle.width;
                let y = rectangle.row.saturating_mul(2) + rectangle.height;
                let in_direction = match direction {
                    WindowDirection::Left => x < current_x,
                    WindowDirection::Right => x > current_x,
                    WindowDirection::Up => y < current_y,
                    WindowDirection::Down => y > current_y,
                };
                if !in_direction {
                    return None;
                }
                let perpendicular_overlap = match direction {
                    WindowDirection::Left | WindowDirection::Right => ranges_overlap(
                        current.row..current.row.saturating_add(current.height),
                        rectangle.row..rectangle.row.saturating_add(rectangle.height),
                    ),
                    WindowDirection::Up | WindowDirection::Down => ranges_overlap(
                        current.column..current.column.saturating_add(current.width),
                        rectangle.column..rectangle.column.saturating_add(rectangle.width),
                    ),
                };
                let (primary, perpendicular) = match direction {
                    WindowDirection::Left | WindowDirection::Right => {
                        (x.abs_diff(current_x), y.abs_diff(current_y))
                    }
                    WindowDirection::Up | WindowDirection::Down => {
                        (y.abs_diff(current_y), x.abs_diff(current_x))
                    }
                };
                Some((window, (!perpendicular_overlap, primary, perpendicular)))
            })
            .min_by_key(|(_, score)| *score)
            .map(|(window, _)| window);
        if let Some(candidate) = candidate {
            self.tabs[tab_index].active_window = candidate;
        }
        Ok(())
    }

    pub fn focus_window_id(&mut self, window_id: WindowId) -> Result<(), ViewModelError> {
        let tab = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == self.active_tab)
            .ok_or(ViewModelError::UnknownTab(self.active_tab))?;
        if !window_ids(&tab.root).contains(&window_id) {
            return Err(ViewModelError::UnknownWindow(window_id));
        }
        tab.active_window = window_id;
        Ok(())
    }

    pub fn cycle_window(&mut self, delta: isize) -> Result<(), ViewModelError> {
        let tab_index = self
            .tabs
            .iter()
            .position(|tab| tab.id == self.active_tab)
            .ok_or(ViewModelError::UnknownTab(self.active_tab))?;
        let ids = window_ids(&self.tabs[tab_index].root);
        let current = ids
            .iter()
            .position(|id| *id == self.tabs[tab_index].active_window)
            .ok_or(ViewModelError::UnknownWindow(
                self.tabs[tab_index].active_window,
            ))?;
        let next = current.wrapping_add_signed(delta).rem_euclid(ids.len());
        self.tabs[tab_index].active_window = ids[next];
        Ok(())
    }

    pub fn only_active_window(&mut self) -> Result<(), ViewModelError> {
        let tab_index = self
            .tabs
            .iter()
            .position(|tab| tab.id == self.active_tab)
            .ok_or(ViewModelError::UnknownTab(self.active_tab))?;
        let active = self.tabs[tab_index].active_window;
        let removed = window_ids(&self.tabs[tab_index].root)
            .into_iter()
            .filter(|window| *window != active)
            .collect::<Vec<_>>();
        self.tabs[tab_index].root = SplitTree::Window(active);
        self.windows.retain(|window| !removed.contains(&window.id));
        Ok(())
    }

    pub fn equalize_windows(&mut self) -> Result<(), ViewModelError> {
        let tab = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == self.active_tab)
            .ok_or(ViewModelError::UnknownTab(self.active_tab))?;
        equalize_split_tree(&mut tab.root);
        Ok(())
    }

    pub fn close_active_tab(&mut self) -> Result<(), ViewModelError> {
        if self.tabs.len() == 1 {
            return Err(ViewModelError::FinalTab);
        }
        let current = self
            .tabs
            .iter()
            .position(|tab| tab.id == self.active_tab)
            .ok_or(ViewModelError::UnknownTab(self.active_tab))?;
        let removed = self.tabs.remove(current);
        let removed_windows = window_ids(&removed.root);
        self.windows
            .retain(|window| !removed_windows.contains(&window.id));
        self.active_tab = self.tabs[current.min(self.tabs.len() - 1)].id;
        Ok(())
    }

    fn allocate(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }
}

fn replace_window_with_split(
    tree: &mut SplitTree,
    target: WindowId,
    new: WindowId,
    axis: SplitAxis,
) -> Result<(), ViewModelError> {
    match tree {
        SplitTree::Window(id) if *id == target => {
            *tree = SplitTree::Split {
                axis,
                ratio_milli: 500,
                first: Box::new(SplitTree::Window(target)),
                second: Box::new(SplitTree::Window(new)),
            };
            Ok(())
        }
        SplitTree::Window(_) => Err(ViewModelError::UnknownWindow(target)),
        SplitTree::Split { first, second, .. } => {
            replace_window_with_split(first, target, new, axis)
                .or_else(|_| replace_window_with_split(second, target, new, axis))
        }
    }
}

fn remove_window(tree: &mut SplitTree, target: WindowId) -> Option<WindowId> {
    let SplitTree::Split { first, second, .. } = tree else {
        return None;
    };
    if matches!(first.as_ref(), SplitTree::Window(id) if *id == target) {
        let sibling = std::mem::replace(second, Box::new(SplitTree::Window(target)));
        let next = first_window(&sibling);
        *tree = *sibling;
        return Some(next);
    }
    if matches!(second.as_ref(), SplitTree::Window(id) if *id == target) {
        let sibling = std::mem::replace(first, Box::new(SplitTree::Window(target)));
        let next = first_window(&sibling);
        *tree = *sibling;
        return Some(next);
    }
    remove_window(first, target).or_else(|| remove_window(second, target))
}

fn first_window(tree: &SplitTree) -> WindowId {
    match tree {
        SplitTree::Window(id) => *id,
        SplitTree::Split { first, .. } => first_window(first),
    }
}

fn window_ids(tree: &SplitTree) -> Vec<WindowId> {
    match tree {
        SplitTree::Window(id) => vec![*id],
        SplitTree::Split { first, second, .. } => {
            let mut ids = window_ids(first);
            ids.extend(window_ids(second));
            ids
        }
    }
}

fn equalize_split_tree(tree: &mut SplitTree) {
    if let SplitTree::Split {
        ratio_milli,
        first,
        second,
        ..
    } = tree
    {
        *ratio_milli = 500;
        equalize_split_tree(first);
        equalize_split_tree(second);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RgbColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl RgbColor {
    #[must_use]
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    #[must_use]
    pub fn from_hex(value: &str) -> Option<Self> {
        let value = value.strip_prefix('#').unwrap_or(value);
        (value.len() == 6).then_some(())?;
        Some(Self {
            red: u8::from_str_radix(&value[0..2], 16).ok()?,
            green: u8::from_str_radix(&value[2..4], 16).ok()?,
            blue: u8::from_str_radix(&value[4..6], 16).ok()?,
        })
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
pub enum CatppuccinFlavor {
    Latte,
    Frappe,
    Macchiato,
    Mocha,
}

impl CatppuccinFlavor {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "latte" | "catppuccin-latte" => Some(Self::Latte),
            "frappe" | "frappé" | "catppuccin-frappe" => Some(Self::Frappe),
            "macchiato" | "catppuccin-macchiato" => Some(Self::Macchiato),
            "mocha" | "catppuccin" | "catppuccin-mocha" => Some(Self::Mocha),
            _ => None,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Latte => "latte",
            Self::Frappe => "frappe",
            Self::Macchiato => "macchiato",
            Self::Mocha => "mocha",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatppuccinPalette {
    pub rosewater: RgbColor,
    pub flamingo: RgbColor,
    pub pink: RgbColor,
    pub mauve: RgbColor,
    pub red: RgbColor,
    pub maroon: RgbColor,
    pub peach: RgbColor,
    pub yellow: RgbColor,
    pub green: RgbColor,
    pub teal: RgbColor,
    pub sky: RgbColor,
    pub sapphire: RgbColor,
    pub blue: RgbColor,
    pub lavender: RgbColor,
    pub text: RgbColor,
    pub subtext1: RgbColor,
    pub subtext0: RgbColor,
    pub overlay2: RgbColor,
    pub overlay1: RgbColor,
    pub overlay0: RgbColor,
    pub surface2: RgbColor,
    pub surface1: RgbColor,
    pub surface0: RgbColor,
    pub base: RgbColor,
    pub mantle: RgbColor,
    pub crust: RgbColor,
}

macro_rules! rgb {
    ($value:literal) => {{
        let value: u32 = $value;
        RgbColor::new(
            ((value >> 16) & 0xff) as u8,
            ((value >> 8) & 0xff) as u8,
            (value & 0xff) as u8,
        )
    }};
}

impl CatppuccinPalette {
    pub const LATTE: Self = Self {
        rosewater: rgb!(0xdc8a78),
        flamingo: rgb!(0xdd7878),
        pink: rgb!(0xea76cb),
        mauve: rgb!(0x8839ef),
        red: rgb!(0xd20f39),
        maroon: rgb!(0xe64553),
        peach: rgb!(0xfe640b),
        yellow: rgb!(0xdf8e1d),
        green: rgb!(0x40a02b),
        teal: rgb!(0x179299),
        sky: rgb!(0x04a5e5),
        sapphire: rgb!(0x209fb5),
        blue: rgb!(0x1e66f5),
        lavender: rgb!(0x7287fd),
        text: rgb!(0x4c4f69),
        subtext1: rgb!(0x5c5f77),
        subtext0: rgb!(0x6c6f85),
        overlay2: rgb!(0x7c7f93),
        overlay1: rgb!(0x8c8fa1),
        overlay0: rgb!(0x9ca0b0),
        surface2: rgb!(0xacb0be),
        surface1: rgb!(0xbcc0cc),
        surface0: rgb!(0xccd0da),
        base: rgb!(0xeff1f5),
        mantle: rgb!(0xe6e9ef),
        crust: rgb!(0xdce0e8),
    };
    pub const FRAPPE: Self = Self {
        rosewater: rgb!(0xf2d5cf),
        flamingo: rgb!(0xeebebe),
        pink: rgb!(0xf4b8e4),
        mauve: rgb!(0xca9ee6),
        red: rgb!(0xe78284),
        maroon: rgb!(0xea999c),
        peach: rgb!(0xef9f76),
        yellow: rgb!(0xe5c890),
        green: rgb!(0xa6d189),
        teal: rgb!(0x81c8be),
        sky: rgb!(0x99d1db),
        sapphire: rgb!(0x85c1dc),
        blue: rgb!(0x8caaee),
        lavender: rgb!(0xbabbf1),
        text: rgb!(0xc6d0f5),
        subtext1: rgb!(0xb5bfe2),
        subtext0: rgb!(0xa5adce),
        overlay2: rgb!(0x949cbb),
        overlay1: rgb!(0x838ba7),
        overlay0: rgb!(0x737994),
        surface2: rgb!(0x626880),
        surface1: rgb!(0x51576d),
        surface0: rgb!(0x414559),
        base: rgb!(0x303446),
        mantle: rgb!(0x292c3c),
        crust: rgb!(0x232634),
    };
    pub const MACCHIATO: Self = Self {
        rosewater: rgb!(0xf4dbd6),
        flamingo: rgb!(0xf0c6c6),
        pink: rgb!(0xf5bde6),
        mauve: rgb!(0xc6a0f6),
        red: rgb!(0xed8796),
        maroon: rgb!(0xee99a0),
        peach: rgb!(0xf5a97f),
        yellow: rgb!(0xeed49f),
        green: rgb!(0xa6da95),
        teal: rgb!(0x8bd5ca),
        sky: rgb!(0x91d7e3),
        sapphire: rgb!(0x7dc4e4),
        blue: rgb!(0x8aadf4),
        lavender: rgb!(0xb7bdf8),
        text: rgb!(0xcad3f5),
        subtext1: rgb!(0xb8c0e0),
        subtext0: rgb!(0xa5adcb),
        overlay2: rgb!(0x939ab7),
        overlay1: rgb!(0x8087a2),
        overlay0: rgb!(0x6e738d),
        surface2: rgb!(0x5b6078),
        surface1: rgb!(0x494d64),
        surface0: rgb!(0x363a4f),
        base: rgb!(0x24273a),
        mantle: rgb!(0x1e2030),
        crust: rgb!(0x181926),
    };
    pub const MOCHA: Self = Self {
        rosewater: rgb!(0xf5e0dc),
        flamingo: rgb!(0xf2cdcd),
        pink: rgb!(0xf5c2e7),
        mauve: rgb!(0xcba6f7),
        red: rgb!(0xf38ba8),
        maroon: rgb!(0xeba0ac),
        peach: rgb!(0xfab387),
        yellow: rgb!(0xf9e2af),
        green: rgb!(0xa6e3a1),
        teal: rgb!(0x94e2d5),
        sky: rgb!(0x89dceb),
        sapphire: rgb!(0x74c7ec),
        blue: rgb!(0x89b4fa),
        lavender: rgb!(0xb4befe),
        text: rgb!(0xcdd6f4),
        subtext1: rgb!(0xbac2de),
        subtext0: rgb!(0xa6adc8),
        overlay2: rgb!(0x9399b2),
        overlay1: rgb!(0x7f849c),
        overlay0: rgb!(0x6c7086),
        surface2: rgb!(0x585b70),
        surface1: rgb!(0x45475a),
        surface0: rgb!(0x313244),
        base: rgb!(0x1e1e2e),
        mantle: rgb!(0x181825),
        crust: rgb!(0x11111b),
    };

    #[must_use]
    pub const fn for_flavor(flavor: CatppuccinFlavor) -> Self {
        match flavor {
            CatppuccinFlavor::Latte => Self::LATTE,
            CatppuccinFlavor::Frappe => Self::FRAPPE,
            CatppuccinFlavor::Macchiato => Self::MACCHIATO,
            CatppuccinFlavor::Mocha => Self::MOCHA,
        }
    }

    #[must_use]
    pub fn set(&mut self, name: &str, color: RgbColor) -> bool {
        let slot = match name {
            "rosewater" => &mut self.rosewater,
            "flamingo" => &mut self.flamingo,
            "pink" => &mut self.pink,
            "mauve" => &mut self.mauve,
            "red" => &mut self.red,
            "maroon" => &mut self.maroon,
            "peach" => &mut self.peach,
            "yellow" => &mut self.yellow,
            "green" => &mut self.green,
            "teal" => &mut self.teal,
            "sky" => &mut self.sky,
            "sapphire" => &mut self.sapphire,
            "blue" => &mut self.blue,
            "lavender" => &mut self.lavender,
            "text" => &mut self.text,
            "subtext1" => &mut self.subtext1,
            "subtext0" => &mut self.subtext0,
            "overlay2" => &mut self.overlay2,
            "overlay1" => &mut self.overlay1,
            "overlay0" => &mut self.overlay0,
            "surface2" => &mut self.surface2,
            "surface1" => &mut self.surface1,
            "surface0" => &mut self.surface0,
            "base" => &mut self.base,
            "mantle" => &mut self.mantle,
            "crust" => &mut self.crust,
            _ => return false,
        };
        *slot = color;
        true
    }

    fn slots(self) -> [RgbColor; 26] {
        [
            self.rosewater,
            self.flamingo,
            self.pink,
            self.mauve,
            self.red,
            self.maroon,
            self.peach,
            self.yellow,
            self.green,
            self.teal,
            self.sky,
            self.sapphire,
            self.blue,
            self.lavender,
            self.text,
            self.subtext1,
            self.subtext0,
            self.overlay2,
            self.overlay1,
            self.overlay0,
            self.surface2,
            self.surface1,
            self.surface0,
            self.base,
            self.mantle,
            self.crust,
        ]
    }

    fn remap_from_mocha(self, color: CellColor) -> CellColor {
        let CellColor::Rgb(rgb) = color else {
            return color;
        };
        Self::MOCHA
            .slots()
            .into_iter()
            .zip(self.slots())
            .find_map(|(source, target)| (source == rgb).then_some(CellColor::Rgb(target)))
            .unwrap_or(color)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub reverse: bool,
    /// True color is preferred; palette indexes remain available for terminal-native colors.
    pub foreground: Option<CellColor>,
    pub background: Option<CellColor>,
}

impl Default for CellStyle {
    fn default() -> Self {
        Self {
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
            reverse: false,
            foreground: Some(CellColor::Rgb(CatppuccinPalette::MOCHA.text)),
            background: Some(CellColor::Rgb(CatppuccinPalette::MOCHA.base)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerOverlayRow {
    pub label: Box<str>,
    pub detail: Box<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerOverlay {
    pub title: Box<str>,
    pub prompt: Box<str>,
    pub rows: Vec<PickerOverlayRow>,
    pub selected: usize,
    pub preview_title: Box<str>,
    pub preview: Box<str>,
    pub preview_scroll: usize,
    pub preview_highlight_line: Option<usize>,
    pub preview_decorations: Vec<DecorationSpan>,
    pub footer: Box<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionOverlayRow {
    pub label: Box<str>,
    pub detail: Box<str>,
    pub source: Box<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionOverlay {
    pub rows: Vec<CompletionOverlayRow>,
    pub selected: Option<usize>,
    pub documentation: Box<str>,
    pub documentation_scroll: usize,
}

#[derive(Clone, Copy)]
struct CompletionMenu {
    rectangle: Rect,
    visible_rows: usize,
    start: usize,
}

#[derive(Clone, Copy)]
struct CompletionStyles {
    base: CellStyle,
    border: CellStyle,
    selected: CellStyle,
}

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

impl TextPopup {
    /// Widths of the display rows used by popup rendering. Keeping navigation
    /// on this calculation prevents the focused cursor from drifting away from
    /// long lines after they wrap.
    #[must_use]
    pub fn navigation_line_widths(&self, terminal_width: usize, tab_width: usize) -> Vec<usize> {
        wrap_popup_text(&self.text, popup_max_inner_width(terminal_width), tab_width)
            .iter()
            .map(|line| display_width(&line.text, tab_width))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AceJumpTarget {
    pub byte: usize,
    pub label: Box<str>,
}

#[derive(Clone, Copy)]
struct AceJumpPane {
    rectangle: Rect,
    gutter: usize,
    content_width: usize,
    start_byte: usize,
    end_byte: usize,
}

struct AceJumpPosition<'a> {
    row: usize,
    column: usize,
    label: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AceJumpOverlay {
    pub targets: Vec<AceJumpTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugOverlay {
    pub scopes: Box<str>,
    pub breakpoints: Box<str>,
    pub stacks: Box<str>,
    pub watches: Box<str>,
    pub repl: Box<str>,
    pub console: Box<str>,
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

// A single Unicode scalar occupies at most four UTF-8 bytes. Longer grapheme
// clusters remain lossless in the heap variant, while ordinary editor cells
// stay compact enough to build and copy a viewport cheaply.
const INLINE_GRAPHEME_BYTES: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellGrapheme {
    Inline {
        len: u8,
        bytes: [u8; INLINE_GRAPHEME_BYTES],
    },
    Heap(Box<str>),
}

impl From<&str> for CellGrapheme {
    fn from(grapheme: &str) -> Self {
        if grapheme.len() <= INLINE_GRAPHEME_BYTES {
            let mut bytes = [0; INLINE_GRAPHEME_BYTES];
            bytes[..grapheme.len()].copy_from_slice(grapheme.as_bytes());
            return Self::Inline {
                len: u8::try_from(grapheme.len()).unwrap_or(0),
                bytes,
            };
        }
        Self::Heap(Box::from(grapheme))
    }
}

impl From<String> for CellGrapheme {
    fn from(grapheme: String) -> Self {
        if grapheme.len() <= INLINE_GRAPHEME_BYTES {
            return Self::from(grapheme.as_str());
        }
        Self::Heap(grapheme.into_boxed_str())
    }
}

impl AsRef<str> for CellGrapheme {
    fn as_ref(&self) -> &str {
        match self {
            Self::Inline { len, bytes } => {
                std::str::from_utf8(&bytes[..usize::from(*len)]).unwrap_or_default()
            }
            Self::Heap(grapheme) => grapheme,
        }
    }
}

impl std::ops::Deref for CellGrapheme {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

impl Serialize for CellGrapheme {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_ref())
    }
}

impl<'de> Deserialize<'de> for CellGrapheme {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CellRow {
    pub cells: Vec<Cell>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesiredGrid {
    pub epoch: u64,
    pub width: usize,
    pub height: usize,
    pub rows: Vec<Arc<CellRow>>,
    pub cursor: (usize, usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedEditorRender {
    text: FrameText,
    content_row_lines: Vec<Option<usize>>,
    top_line: usize,
    width: usize,
    height: usize,
    tab_width: usize,
    line_numbers: bool,
    relative_numbers: bool,
    number_width: usize,
    color_column: Option<usize>,
    theme: CatppuccinPalette,
    status: Box<str>,
    prompt: Option<Box<str>>,
    decorations: SharedDecorations,
    line_decorations: Vec<LineDecoration>,
    relative_cursor_line: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedLogicalRow {
    row: Arc<CellRow>,
    decorations: Vec<DecorationSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedEditorViewport {
    render: CachedEditorRender,
    rows: Vec<Arc<CellRow>>,
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

struct EditorRenderRequest<'a> {
    frame: &'a EngineFrame,
    status: &'a str,
    prompt: Option<&'a str>,
    decorations: &'a [DecorationSpan],
    line_decorations: &'a [LineDecoration],
    shared: Option<&'a SharedDecorations>,
}

struct WorkspacePaneSource<'a> {
    window: &'a WindowView,
    frame: &'a EngineFrame,
    decorations: &'a SharedDecorations,
    line_decorations: &'a [LineDecoration],
}

struct ViewportShift {
    moving_down: bool,
    line_delta: usize,
    content_height: usize,
    introduced_start: usize,
    introduced_end: usize,
    start_byte: usize,
    end_byte: usize,
    cursor_line: usize,
    cached_row_lines: Vec<Option<usize>>,
    cached_decorations: SharedDecorations,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalPatch {
    Clear,
    ClearToEndOfLine(CellStyle),
    MoveTo { column: usize, row: usize },
    SetStyle(CellStyle),
    Put(Cell),
    PutRow(Arc<CellRow>),
    ShowCursor(bool),
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
    cached_logical_rows: BTreeMap<usize, CachedLogicalRow>,
    cached_editor_render: Option<CachedEditorRender>,
    cached_editor_viewports: Vec<CachedEditorViewport>,
    cached_status_rows: Vec<(StatusOverlay, Arc<CellRow>)>,
    pane_layouts: BTreeMap<WindowId, Box<ViewportLayout>>,
    realtime_updates_prepared: bool,
}

impl ViewportLayout {
    #[must_use]
    pub const fn new(width: usize, height: usize) -> Self {
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
            theme: CatppuccinPalette::MOCHA,
            epoch: 0,
            cached_rows: Vec::new(),
            cached_logical_rows: BTreeMap::new(),
            cached_editor_render: None,
            cached_editor_viewports: Vec::new(),
            cached_status_rows: Vec::new(),
            pane_layouts: BTreeMap::new(),
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

    #[must_use]
    pub const fn theme(&self) -> CatppuccinPalette {
        self.theme
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

    fn clear_cached_rendering(&mut self) {
        self.cached_rows.clear();
        self.cached_logical_rows.clear();
        self.cached_editor_render = None;
        self.cached_editor_viewports.clear();
        self.cached_status_rows.clear();
        self.pane_layouts.clear();
    }

    /// Maps a terminal cell back through the same pane, gutter, tab, Unicode,
    /// and soft-wrap geometry used to render the workspace.
    #[must_use]
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
        if column >= self.width || row >= content_height {
            return None;
        }
        let mut panes = Vec::new();
        split_rectangles(
            &model.active_tab().root,
            Rect {
                column: 0,
                row: 0,
                width: self.width,
                height: content_height,
            },
            &mut panes,
        );
        let (window_id, rectangle) = panes.into_iter().find(|(_, rectangle)| {
            column >= rectangle.column
                && column < rectangle.column.saturating_add(rectangle.width)
                && row >= rectangle.row
                && row < rectangle.row.saturating_add(rectangle.height)
        })?;
        let window = model.windows.iter().find(|window| window.id == window_id)?;
        let (_, frame) = frames
            .iter()
            .find(|(buffer_id, _)| *buffer_id == window.buffer_id)?;
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
        let gutter = if pane.line_numbers {
            pane.number_width.min(pane.width.saturating_sub(1))
        } else {
            0
        };
        let content_width = pane.width.saturating_sub(gutter).max(1);
        let local_column = column
            .saturating_sub(rectangle.column)
            .saturating_sub(gutter);
        let local_row = row.saturating_sub(rectangle.row);
        let start_byte = self.cached_frame_byte_of_line(frame, pane.top_line);
        let byte = byte_at_visual_cell(
            &frame.text,
            start_byte,
            content_width,
            pane.tab_width,
            local_column,
            local_row,
        )?;
        Some(EditorHit {
            window_id,
            buffer_id: window.buffer_id,
            byte,
        })
    }

    pub fn ensure_cursor_visible(&mut self, frame: &EngineFrame, reserved_rows: usize) {
        let cursor = frame.cursor_byte.min(frame.text.len());
        let cursor_line = self.cached_frame_line_of_byte(frame, cursor);
        let content_height = self.height.saturating_sub(reserved_rows).max(1);
        let margin = self
            .scrolloff
            .min(content_height.saturating_sub(1).checked_div(2).unwrap_or(0));
        if cursor_line < self.top_line.saturating_add(margin) {
            self.top_line = cursor_line.saturating_sub(margin);
            self.epoch = self.epoch.saturating_add(1);
        } else if cursor_line.saturating_add(margin) >= self.top_line + content_height {
            self.top_line = cursor_line
                .saturating_add(margin)
                .saturating_add(1)
                .saturating_sub(content_height);
            self.epoch = self.epoch.saturating_add(1);
        }
    }

    fn cached_frame_byte_of_line(&self, frame: &EngineFrame, line: usize) -> usize {
        frame.text.byte_of_line(line)
    }

    fn cached_frame_line_of_byte(&self, frame: &EngineFrame, byte: usize) -> usize {
        frame.text.line_of_byte(byte)
    }

    #[must_use]
    pub fn desired_grid(&mut self, frame: &EngineFrame) -> DesiredGrid {
        self.desired_editor_grid_with_decorations(frame, "", None, &[])
    }

    #[must_use]
    pub fn desired_editor_grid(
        &mut self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
    ) -> DesiredGrid {
        self.desired_editor_grid_with_decorations(frame, status, prompt, &[])
    }

    #[must_use]
    pub fn desired_editor_grid_with_decorations(
        &mut self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &[DecorationSpan],
    ) -> DesiredGrid {
        self.desired_editor_grid_with_line_decorations(frame, status, prompt, decorations, &[])
    }

    #[must_use]
    pub fn desired_editor_grid_with_line_decorations(
        &mut self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) -> DesiredGrid {
        self.desired_editor_grid_retaining_decorations(
            frame,
            status,
            prompt,
            decorations,
            line_decorations,
            None,
        )
    }

    #[must_use]
    pub fn desired_editor_grid_with_shared_decorations(
        &mut self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &SharedDecorations,
        line_decorations: &[LineDecoration],
    ) -> DesiredGrid {
        self.desired_editor_grid_retaining_decorations(
            frame,
            status,
            prompt,
            decorations,
            line_decorations,
            Some(decorations),
        )
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
        if let Some(grid) = self.reuse_cached_editor_grid(
            frame,
            status,
            prompt,
            decorations,
            line_decorations,
            shared,
        ) {
            return grid;
        }
        self.render_editor_grid(frame, status, prompt, decorations, line_decorations, shared)
    }

    fn reuse_cached_editor_grid(
        &mut self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
        shared: Option<&SharedDecorations>,
    ) -> Option<DesiredGrid> {
        // Text changes and presentation-only changes have disjoint reuse
        // strategies. Classify that state once so an edit does not walk every
        // viewport, decoration, and cursor-only cache before reaching the
        // incremental line renderer.
        if self
            .cached_editor_render
            .as_ref()
            .is_some_and(|cached| !cached.text.same_snapshot(&frame.text))
        {
            return self.update_cached_editor_line(
                frame,
                status,
                prompt,
                decorations,
                line_decorations,
                shared,
            );
        }
        if self.editor_render_is_cached(frame, status, prompt, decorations, line_decorations) {
            return Some(self.reuse_editor_render(frame, status, prompt));
        }
        if let Some(grid) =
            self.reuse_recent_editor_viewport(frame, status, prompt, decorations, line_decorations)
        {
            return Some(grid);
        }
        self.stash_current_editor_viewport(frame);
        if let Some(grid) = self.update_cached_editor_distant_viewport(
            frame,
            status,
            prompt,
            decorations,
            line_decorations,
        ) {
            return Some(grid);
        }
        if let Some(grid) =
            self.update_cached_editor_viewport(frame, status, prompt, decorations, line_decorations)
        {
            return Some(grid);
        }
        if let Some(grid) = self.update_cached_editor_decorations(
            frame,
            status,
            prompt,
            decorations,
            line_decorations,
            shared,
        ) {
            return Some(grid);
        }
        if let Some(grid) = self.update_cached_editor_relative_numbers(
            frame,
            status,
            prompt,
            decorations,
            line_decorations,
        ) {
            return Some(grid);
        }
        None
    }

    fn render_editor_grid(
        &mut self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
        shared: Option<&SharedDecorations>,
    ) -> DesiredGrid {
        let preserve_logical_rows = self.cached_editor_render.as_ref().is_some_and(|cached| {
            cached.text.same_snapshot(&frame.text)
                && self.editor_render_static_structure_is_cached(status, prompt, line_decorations)
        });
        if !preserve_logical_rows {
            self.cached_logical_rows.clear();
        }
        let layout = self.editor_grid_layout(status, prompt);
        let mut content = self.render_editor_content(frame, decorations, line_decorations, layout);
        self.append_editor_footer(
            &mut content.rows,
            &mut content.cursor,
            layout.content_height,
            status,
            prompt,
        );
        self.finish_editor_grid(
            EditorRenderRequest {
                frame,
                status,
                prompt,
                decorations,
                line_decorations,
                shared,
            },
            content,
        )
    }

    fn editor_grid_layout(&self, status: &str, prompt: Option<&str>) -> EditorGridLayout {
        let reserved_rows = usize::from(!status.is_empty() || prompt.is_some());
        let content_height = self.height.saturating_sub(reserved_rows).max(1);
        let gutter = if self.line_numbers {
            self.number_width.min(self.width.saturating_sub(1))
        } else {
            0
        };
        EditorGridLayout {
            content_height,
            gutter,
        }
    }

    fn render_editor_content(
        &self,
        frame: &EngineFrame,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
        layout: EditorGridLayout,
    ) -> RenderedEditorContent {
        let start_byte = self.cached_frame_byte_of_line(frame, self.top_line);
        let mut builder = GridBuilder::new(
            self.width.saturating_sub(layout.gutter),
            layout.content_height,
            self.tab_width,
            start_byte,
            self.top_line,
        );
        let end_byte = frame
            .text
            .byte_of_line(self.top_line.saturating_add(layout.content_height));
        let visible = frame.text.slice(start_byte..end_byte);
        builder.push_grapheme_document(&visible, frame.cursor_byte, decorations);
        let mut row_lines = builder.row_lines.clone();
        row_lines.resize(layout.content_height, None);
        row_lines.truncate(layout.content_height);
        let mut rows = builder.rows;
        let mut cursor = builder.cursor.unwrap_or((0, 0));
        if layout.gutter > 0 {
            let cursor_line = self.cached_frame_line_of_byte(frame, frame.cursor_byte);
            prepend_line_numbers(
                &mut rows,
                &builder.row_lines,
                cursor_line,
                layout.gutter,
                self.relative_numbers,
                line_decorations,
            );
            cursor.0 = cursor.0.saturating_add(layout.gutter);
        }
        if let Some(column) = self.color_column {
            mark_color_column(
                &mut rows,
                layout.gutter.saturating_add(column.saturating_sub(1)),
                self.width,
                self.theme.mantle,
            );
        }
        RenderedEditorContent {
            rows,
            row_lines,
            cursor,
        }
    }

    fn append_editor_footer(
        &self,
        rows: &mut Vec<CellRow>,
        cursor: &mut (usize, usize),
        content_height: usize,
        status: &str,
        prompt: Option<&str>,
    ) {
        let Some(label) = prompt.or((!status.is_empty()).then_some(status)) else {
            return;
        };
        let style = prompt_style(prompt.is_some());
        rows.resize_with(content_height, CellRow::default);
        rows.push(row_from_text(label, self.width, style, self.tab_width));
        if prompt.is_some() {
            *cursor = self.prompt_cursor(label);
        }
    }

    fn finish_editor_grid(
        &mut self,
        request: EditorRenderRequest<'_>,
        mut content: RenderedEditorContent,
    ) -> DesiredGrid {
        content.rows.resize_with(self.height, CellRow::default);
        content.rows.truncate(self.height);
        ensure_row_backgrounds(&mut content.rows);
        apply_theme_to_rows(&mut content.rows, self.theme);
        let rows: Vec<_> = content
            .rows
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                self.cached_rows
                    .get(index)
                    .filter(|cached| cached.as_ref() == &row)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(row))
            })
            .collect();
        self.cached_rows.clone_from(&rows);
        self.cached_editor_render = Some(CachedEditorRender {
            text: request.frame.text.clone(),
            content_row_lines: content.row_lines,
            top_line: self.top_line,
            width: self.width,
            height: self.height,
            tab_width: self.tab_width,
            line_numbers: self.line_numbers,
            relative_numbers: self.relative_numbers,
            number_width: self.number_width,
            color_column: self.color_column,
            theme: self.theme,
            status: request.status.into(),
            prompt: request.prompt.map(Into::into),
            decorations: retain_decorations(request.decorations, request.shared),
            line_decorations: request.line_decorations.to_vec(),
            relative_cursor_line: self
                .relative_numbers
                .then(|| self.cached_frame_line_of_byte(request.frame, request.frame.cursor_byte)),
        });
        self.epoch = self.epoch.saturating_add(1);
        DesiredGrid {
            epoch: self.epoch,
            width: self.width,
            height: self.height,
            rows,
            cursor: content.cursor,
        }
    }

    fn prompt_cursor(&self, label: &str) -> (usize, usize) {
        let input = label.split("  │  ").next().unwrap_or(label);
        (
            display_width(input, self.tab_width).min(self.width.saturating_sub(1)),
            self.height.saturating_sub(1),
        )
    }

    fn editor_render_is_cached(
        &self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) -> bool {
        let Some(cached) = &self.cached_editor_render else {
            return false;
        };
        cached.text.same_snapshot(&frame.text)
            && self.editor_render_context_is_cached(
                frame,
                status,
                prompt,
                decorations,
                line_decorations,
            )
    }

    fn reuse_recent_editor_viewport(
        &mut self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) -> Option<DesiredGrid> {
        self.cached_editor_viewports
            .retain(|viewport| viewport.render.text.same_snapshot(&frame.text));
        let cursor_line = frame.text.line_of_byte(frame.cursor_byte);
        let index = self.cached_editor_viewports.iter().position(|viewport| {
            let cached = &viewport.render;
            viewport.rows.len() == self.height
                && cached.top_line == self.top_line
                && cached.text.same_snapshot(&frame.text)
                && cached.width == self.width
                && cached.height == self.height
                && cached.tab_width == self.tab_width
                && cached.line_numbers == self.line_numbers
                && cached.relative_numbers == self.relative_numbers
                && cached.number_width == self.number_width
                && cached.color_column == self.color_column
                && cached.theme == self.theme
                && cached.status.as_ref() == status
                && cached.prompt.as_deref() == prompt
                && cached.decorations.as_ref() == decorations
                && cached.line_decorations == line_decorations
                && cached.relative_cursor_line == self.relative_numbers.then_some(cursor_line)
        })?;
        let viewport = self.cached_editor_viewports.swap_remove(index);
        if let Some(render) = self.cached_editor_render.take()
            && self.cached_rows.len() == self.height
            && render.text.same_snapshot(&frame.text)
        {
            self.cached_editor_viewports.push(CachedEditorViewport {
                render,
                rows: std::mem::take(&mut self.cached_rows),
            });
        }
        self.cached_editor_render = Some(viewport.render);
        self.cached_rows = viewport.rows;
        self.prune_recent_editor_viewports();
        Some(self.reuse_editor_render(frame, status, prompt))
    }

    fn stash_current_editor_viewport(&mut self, frame: &EngineFrame) {
        let Some(cached) = self.cached_editor_render.as_ref() else {
            return;
        };
        if self.cached_rows.len() != self.height || !cached.text.same_snapshot(&frame.text) {
            self.cached_editor_viewports
                .retain(|viewport| viewport.render.text.same_snapshot(&frame.text));
            return;
        }
        let duplicate = self.cached_editor_viewports.iter().any(|viewport| {
            viewport.render.top_line == cached.top_line
                && viewport.render.relative_cursor_line == cached.relative_cursor_line
                && viewport.render.decorations == cached.decorations
                && viewport.render.line_decorations == cached.line_decorations
                && viewport.render.status == cached.status
                && viewport.render.prompt == cached.prompt
        });
        if !duplicate {
            self.cached_editor_viewports.push(CachedEditorViewport {
                render: cached.clone(),
                rows: self.cached_rows.clone(),
            });
            self.prune_recent_editor_viewports();
        }
    }

    fn prune_recent_editor_viewports(&mut self) {
        const RECENT_VIEWPORT_CAPACITY: usize = 8;
        if self.cached_editor_viewports.len() > RECENT_VIEWPORT_CAPACITY {
            self.cached_editor_viewports
                .drain(..self.cached_editor_viewports.len() - RECENT_VIEWPORT_CAPACITY);
        }
    }

    fn editor_render_context_is_cached(
        &self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) -> bool {
        let Some(cached) = &self.cached_editor_render else {
            return false;
        };
        self.cached_rows.len() == self.height
            && cached.top_line == self.top_line
            && self.editor_render_stable_context_is_cached(
                frame,
                status,
                prompt,
                decorations,
                line_decorations,
            )
    }

    fn editor_render_stable_context_is_cached(
        &self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) -> bool {
        let Some(cached) = &self.cached_editor_render else {
            return false;
        };
        self.editor_render_structure_is_cached(frame, status, prompt, line_decorations)
            && cached.decorations.as_ref() == decorations
    }

    fn editor_render_structure_is_cached(
        &self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        line_decorations: &[LineDecoration],
    ) -> bool {
        self.editor_render_static_structure_is_cached(status, prompt, line_decorations)
            && self.cached_editor_render.as_ref().is_some_and(|cached| {
                cached.relative_cursor_line
                    == self
                        .relative_numbers
                        .then(|| frame.text.line_of_byte(frame.cursor_byte))
            })
    }

    fn editor_render_static_structure_is_cached(
        &self,
        status: &str,
        prompt: Option<&str>,
        line_decorations: &[LineDecoration],
    ) -> bool {
        self.editor_render_shape_is_cached(prompt, line_decorations)
            && self
                .cached_editor_render
                .as_ref()
                .is_some_and(|cached| cached.status.as_ref() == status)
    }

    fn editor_render_shape_is_cached(
        &self,
        prompt: Option<&str>,
        line_decorations: &[LineDecoration],
    ) -> bool {
        let Some(cached) = &self.cached_editor_render else {
            return false;
        };
        self.cached_rows.len() == self.height
            && cached.width == self.width
            && cached.height == self.height
            && cached.tab_width == self.tab_width
            && cached.line_numbers == self.line_numbers
            && cached.relative_numbers == self.relative_numbers
            && cached.number_width == self.number_width
            && cached.color_column == self.color_column
            && cached.theme == self.theme
            && cached.prompt.as_deref() == prompt
            && cached.line_decorations == line_decorations
    }

    fn editor_line_update_context_is_cached(
        &self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        line_decorations: &[LineDecoration],
    ) -> bool {
        let Some(cached) = &self.cached_editor_render else {
            return false;
        };
        self.editor_render_shape_is_cached(prompt, line_decorations)
            && cached.top_line == self.top_line
            && cached.status.is_empty() == status.is_empty()
            && cached.relative_cursor_line
                == self
                    .relative_numbers
                    .then(|| frame.text.line_of_byte(frame.cursor_byte))
    }

    fn update_cached_editor_decorations(
        &mut self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
        shared: Option<&SharedDecorations>,
    ) -> Option<DesiredGrid> {
        let cached = self.cached_editor_render.as_ref()?;
        if !cached.text.same_snapshot(&frame.text)
            || cached.top_line != self.top_line
            || cached.decorations.as_ref() == decorations
            || !self.editor_render_structure_is_cached(frame, status, prompt, line_decorations)
        {
            return None;
        }
        let prefix = cached
            .decorations
            .iter()
            .zip(decorations)
            .take_while(|(old, new)| old == new)
            .count();
        let suffix = cached.decorations[prefix..]
            .iter()
            .rev()
            .zip(decorations[prefix.min(decorations.len())..].iter().rev())
            .take_while(|(old, new)| old == new)
            .count();
        let old_end = cached.decorations.len().saturating_sub(suffix);
        let new_end = decorations.len().saturating_sub(suffix);
        let changed = cached.decorations[prefix.min(old_end)..old_end]
            .iter()
            .chain(decorations[prefix.min(new_end)..new_end].iter());
        let affected = changed.fold(None::<Range<usize>>, |affected, span| {
            Some(affected.map_or_else(
                || span.range.clone(),
                |affected| affected.start.min(span.range.start)..affected.end.max(span.range.end),
            ))
        })?;
        let logical_line = frame.text.line_of_byte(affected.start);
        if frame
            .text
            .line_of_byte(affected.end.saturating_sub(1).max(affected.start))
            != logical_line
        {
            return None;
        }
        self.cached_logical_rows.remove(&logical_line);
        let Some(screen_row) = cached
            .content_row_lines
            .iter()
            .position(|line| *line == Some(logical_line))
        else {
            self.cached_editor_render.as_mut()?.decorations =
                retain_decorations(decorations, shared);
            return Some(self.reuse_editor_render(frame, status, prompt));
        };
        let visible_rows = cached.content_row_lines[screen_row..]
            .iter()
            .take_while(|line| **line == Some(logical_line))
            .count()
            .max(1);
        let start = frame.text.byte_of_line(logical_line);
        let mut end = frame.text.byte_of_line(logical_line.saturating_add(1));
        if end > start && frame.text.slice(end - 1..end).as_ref() == "\n" {
            end -= 1;
        }
        let cursor_line = frame.text.line_of_byte(frame.cursor_byte);
        let rendered = self.render_editor_line(
            &frame.text,
            logical_line,
            cursor_line,
            start,
            end,
            visible_rows,
            decorations,
            line_decorations,
        );
        if rendered.len() != visible_rows {
            return None;
        }
        for (offset, row) in rendered.into_iter().enumerate() {
            self.cached_rows[screen_row + offset] = Arc::new(row);
        }
        self.cached_editor_render.as_mut()?.decorations = retain_decorations(decorations, shared);
        Some(self.reuse_editor_render(frame, status, prompt))
    }

    fn update_cached_editor_relative_numbers(
        &mut self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) -> Option<DesiredGrid> {
        let cached = self.cached_editor_render.as_ref()?;
        let cursor_line = frame.text.line_of_byte(frame.cursor_byte);
        if !self.relative_numbers
            || !cached.text.same_snapshot(&frame.text)
            || cached.top_line != self.top_line
            || cached.decorations.as_ref() != decorations
            || cached.relative_cursor_line == Some(cursor_line)
            || !self.editor_render_static_structure_is_cached(status, prompt, line_decorations)
        {
            return None;
        }
        let content_row_lines = cached.content_row_lines.clone();
        for (row, logical_line) in self.cached_rows.iter_mut().zip(content_row_lines) {
            let Some(logical_line) = logical_line else {
                continue;
            };
            replace_line_number(
                Arc::make_mut(row),
                logical_line,
                cursor_line,
                self.number_width,
                true,
                line_decorations,
            );
        }
        self.cached_editor_render.as_mut()?.relative_cursor_line = Some(cursor_line);
        Some(self.reuse_editor_render(frame, status, prompt))
    }

    fn update_cached_editor_viewport(
        &mut self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) -> Option<DesiredGrid> {
        let shift = self.viewport_shift(frame, status, prompt, decorations, line_decorations)?;
        let introduced = self.shift_introduced_rows(frame, decorations, line_decorations, &shift);
        if introduced.len() != shift.line_delta {
            return None;
        }
        self.stash_shift_displaced_rows(frame, &shift);
        self.install_shift_rows(&shift, introduced);
        self.commit_viewport_shift(&shift, decorations, line_decorations)?;
        self.prune_cached_logical_rows(shift.content_height);
        Some(self.reuse_editor_render(frame, status, prompt))
    }

    fn viewport_shift(
        &self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) -> Option<ViewportShift> {
        let cached = self.cached_editor_render.as_ref()?;
        let cached_top_line = cached.top_line;
        let moving_down = self.top_line >= cached_top_line;
        let line_delta = self.top_line.abs_diff(cached_top_line);
        if !cached.text.same_snapshot(&frame.text)
            || line_delta == 0
            || !self.editor_render_static_structure_is_cached(status, prompt, line_decorations)
        {
            return None;
        }
        let reserved_rows = usize::from(!status.is_empty() || prompt.is_some());
        let content_height = self.height.saturating_sub(reserved_rows).max(1);
        if line_delta >= content_height
            || cached.content_row_lines.len() != content_height
            || !cached
                .content_row_lines
                .iter()
                .enumerate()
                .all(|(offset, line)| *line == Some(cached_top_line.saturating_add(offset)))
        {
            return None;
        }
        let cached_content_row_lines = cached.content_row_lines.clone();
        let cached_decorations = cached.decorations.clone();
        let overlap_start_line = self.top_line.max(cached_top_line);
        let overlap_end_line = self
            .top_line
            .saturating_add(content_height)
            .min(cached_top_line.saturating_add(content_height));
        let overlap =
            frame.text.byte_of_line(overlap_start_line)..frame.text.byte_of_line(overlap_end_line);
        if !decorations_match_range(&cached_decorations, decorations, &overlap) {
            return None;
        }
        let cursor_line = self.cached_frame_line_of_byte(frame, frame.cursor_byte);
        let introduced_start = if moving_down {
            self.top_line
                .saturating_add(content_height.saturating_sub(line_delta))
        } else {
            self.top_line
        };
        let introduced_end = introduced_start.saturating_add(line_delta);
        let start = self.cached_frame_byte_of_line(frame, introduced_start);
        let end = frame
            .text
            .byte_of_line(introduced_end)
            .min(frame.text.len());
        let text = frame.text.slice(start..end);
        let content_width = self
            .width
            .saturating_sub(self.editor_grid_layout(status, prompt).gutter)
            .max(1);
        if text
            .split_terminator('\n')
            .any(|line| visual_line_rows(line, content_width, self.tab_width) != 1)
        {
            return None;
        }
        Some(ViewportShift {
            moving_down,
            line_delta,
            content_height,
            introduced_start,
            introduced_end,
            start_byte: start,
            end_byte: end,
            cursor_line,
            cached_row_lines: cached_content_row_lines,
            cached_decorations,
        })
    }

    fn shift_introduced_rows(
        &mut self,
        frame: &EngineFrame,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
        shift: &ViewportShift,
    ) -> Vec<Arc<CellRow>> {
        (shift.introduced_start..shift.introduced_end)
            .map(|line| {
                let range =
                    frame.text.byte_of_line(line)..frame.text.byte_of_line(line.saturating_add(1));
                let cached = self.cached_logical_rows.remove(&line)?;
                decorations_match_range(&cached.decorations, decorations, &range)
                    .then_some(cached.row)
            })
            .collect::<Option<Vec<_>>>()
            .unwrap_or_else(|| {
                self.render_editor_line(
                    &frame.text,
                    shift.introduced_start,
                    shift.cursor_line,
                    shift.start_byte,
                    shift.end_byte,
                    shift.line_delta,
                    decorations,
                    line_decorations,
                )
                .into_iter()
                .map(Arc::new)
                .collect()
            })
    }

    fn stash_shift_displaced_rows(&mut self, frame: &EngineFrame, shift: &ViewportShift) {
        let displaced = if shift.moving_down {
            0..shift.line_delta
        } else {
            shift.content_height.saturating_sub(shift.line_delta)..shift.content_height
        };
        for index in displaced {
            if let (Some(Some(line)), Some(row)) = (
                shift.cached_row_lines.get(index),
                self.cached_rows.get(index),
            ) {
                let range =
                    frame.text.byte_of_line(*line)..frame.text.byte_of_line(line.saturating_add(1));
                self.cached_logical_rows.insert(
                    *line,
                    CachedLogicalRow {
                        row: Arc::clone(row),
                        decorations: shift
                            .cached_decorations
                            .iter()
                            .filter(|span| {
                                span.range.start < range.end && range.start < span.range.end
                            })
                            .cloned()
                            .collect(),
                    },
                );
            }
        }
    }

    fn install_shift_rows(&mut self, shift: &ViewportShift, introduced: Vec<Arc<CellRow>>) {
        if shift.moving_down {
            self.cached_rows[..shift.content_height].rotate_left(shift.line_delta);
            for (offset, row) in introduced.into_iter().enumerate() {
                self.cached_rows[shift.content_height - shift.line_delta + offset] = row;
            }
        } else {
            self.cached_rows[..shift.content_height].rotate_right(shift.line_delta);
            for (offset, row) in introduced.into_iter().enumerate() {
                self.cached_rows[offset] = row;
            }
        }
    }

    fn commit_viewport_shift(
        &mut self,
        shift: &ViewportShift,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) -> Option<()> {
        let cached = self.cached_editor_render.as_mut()?;
        cached.top_line = self.top_line;
        for (offset, line) in cached.content_row_lines.iter_mut().enumerate() {
            *line = Some(self.top_line.saturating_add(offset));
        }
        cached.relative_cursor_line = self.relative_numbers.then_some(shift.cursor_line);
        cached.decorations = Arc::new(decorations.to_vec());
        if self.relative_numbers {
            for (row, logical_line) in self
                .cached_rows
                .iter_mut()
                .zip(cached.content_row_lines.iter().copied())
            {
                let Some(logical_line) = logical_line else {
                    continue;
                };
                replace_line_number(
                    Arc::make_mut(row),
                    logical_line,
                    shift.cursor_line,
                    self.number_width,
                    true,
                    line_decorations,
                );
            }
        }
        Some(())
    }

    fn update_cached_editor_distant_viewport(
        &mut self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) -> Option<DesiredGrid> {
        let content_height =
            self.distant_viewport_height(frame, status, prompt, line_decorations)?;
        self.stash_current_viewport_rows(frame);
        let target_rows = self.cached_viewport_rows(frame, decorations, content_height)?;
        for (target, row) in self.cached_rows[..content_height]
            .iter_mut()
            .zip(target_rows)
        {
            *target = row;
        }
        let cursor_line = frame.text.line_of_byte(frame.cursor_byte);
        if self.relative_numbers {
            for (offset, row) in self.cached_rows[..content_height].iter_mut().enumerate() {
                replace_line_number(
                    Arc::make_mut(row),
                    self.top_line.saturating_add(offset),
                    cursor_line,
                    self.number_width,
                    true,
                    line_decorations,
                );
            }
        }
        let cached = self.cached_editor_render.as_mut()?;
        cached.top_line = self.top_line;
        for (offset, line) in cached.content_row_lines.iter_mut().enumerate() {
            *line = Some(self.top_line.saturating_add(offset));
        }
        cached.relative_cursor_line = self.relative_numbers.then_some(cursor_line);
        cached.decorations = Arc::new(decorations.to_vec());
        self.prune_cached_logical_rows(content_height);
        Some(self.reuse_editor_render(frame, status, prompt))
    }

    fn distant_viewport_height(
        &self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        line_decorations: &[LineDecoration],
    ) -> Option<usize> {
        let cached = self.cached_editor_render.as_ref()?;
        let reserved_rows = usize::from(!status.is_empty() || prompt.is_some());
        let content_height = self.height.saturating_sub(reserved_rows).max(1);
        if !(self.top_line.abs_diff(cached.top_line) >= content_height
            && cached.text.same_snapshot(&frame.text)
            && cached.content_row_lines.len() == content_height
            && self.editor_render_static_structure_is_cached(status, prompt, line_decorations)
            && cached
                .content_row_lines
                .iter()
                .enumerate()
                .all(|(offset, line)| *line == Some(cached.top_line.saturating_add(offset))))
        {
            return None;
        }
        (self.top_line..self.top_line.saturating_add(content_height))
            .all(|line| self.cached_logical_rows.contains_key(&line))
            .then_some(content_height)
    }

    fn stash_current_viewport_rows(&mut self, frame: &EngineFrame) {
        let Some(cached) = &self.cached_editor_render else {
            return;
        };
        for (index, logical_line) in cached.content_row_lines.iter().copied().enumerate() {
            let (Some(logical_line), Some(row)) = (logical_line, self.cached_rows.get(index))
            else {
                continue;
            };
            let range = frame.text.byte_of_line(logical_line)
                ..frame.text.byte_of_line(logical_line.saturating_add(1));
            self.cached_logical_rows.insert(
                logical_line,
                CachedLogicalRow {
                    row: Arc::clone(row),
                    decorations: cached
                        .decorations
                        .iter()
                        .filter(|span| span.range.start < range.end && range.start < span.range.end)
                        .cloned()
                        .collect(),
                },
            );
        }
    }

    fn cached_viewport_rows(
        &self,
        frame: &EngineFrame,
        decorations: &[DecorationSpan],
        content_height: usize,
    ) -> Option<Vec<Arc<CellRow>>> {
        (self.top_line..self.top_line.saturating_add(content_height))
            .map(|logical_line| {
                let range = frame.text.byte_of_line(logical_line)
                    ..frame.text.byte_of_line(logical_line.saturating_add(1));
                let row = self.cached_logical_rows.get(&logical_line)?;
                decorations_match_range(&row.decorations, decorations, &range)
                    .then(|| Arc::clone(&row.row))
            })
            .collect()
    }

    fn prune_cached_logical_rows(&mut self, content_height: usize) {
        let capacity = content_height.saturating_mul(8).max(content_height);
        while self.cached_logical_rows.len() > capacity {
            let Some(farthest) = self
                .cached_logical_rows
                .keys()
                .max_by_key(|line| line.abs_diff(self.top_line))
                .copied()
            else {
                break;
            };
            self.cached_logical_rows.remove(&farthest);
        }
    }

    fn update_cached_editor_line(
        &mut self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
        shared: Option<&SharedDecorations>,
    ) -> Option<DesiredGrid> {
        if !self.editor_line_update_context_is_cached(frame, status, prompt, line_decorations) {
            return None;
        }
        let cached = self.cached_editor_render.as_ref()?;
        let change = single_line_change(&cached.text, &frame.text)?;
        if !decorations_match_outside_change(&cached.decorations, decorations, change) {
            return None;
        }
        self.cached_logical_rows.clear();
        let reserved_rows = usize::from(!status.is_empty() || prompt.is_some());
        let content_height = self.height.saturating_sub(reserved_rows).max(1);
        if change.line < self.top_line
            || change.line >= self.top_line.saturating_add(content_height)
        {
            self.cached_editor_render
                .as_mut()?
                .text
                .clone_from(&frame.text);
            return Some(self.reuse_editor_render(frame, status, prompt));
        }
        let gutter = if self.line_numbers {
            self.number_width.min(self.width.saturating_sub(1))
        } else {
            0
        };
        let content_width = self.width.saturating_sub(gutter).max(1);
        let screen_row = cached
            .content_row_lines
            .iter()
            .position(|line| *line == Some(change.line))?;
        let old_rows = visual_line_rows(
            &change.old_text(&cached.text),
            content_width,
            self.tab_width,
        );
        let new_rows =
            visual_line_rows(&change.new_text(&frame.text), content_width, self.tab_width);
        if old_rows != new_rows {
            return None;
        }
        let cursor_line = self.cached_frame_line_of_byte(frame, frame.cursor_byte);
        self.repaint_cached_editor_line(
            frame,
            LineRepaint {
                change,
                cursor_line,
                screen_row,
                content_height,
                new_rows,
            },
            decorations,
            line_decorations,
        )?;
        let cached = self.cached_editor_render.as_mut()?;
        cached.text.clone_from(&frame.text);
        cached.status = status.into();
        cached.decorations = retain_decorations(decorations, shared);
        cached.relative_cursor_line = self.relative_numbers.then_some(cursor_line);
        self.update_cached_editor_footer(status, prompt);
        Some(self.reuse_editor_render(frame, status, prompt))
    }

    fn repaint_cached_editor_line(
        &mut self,
        frame: &EngineFrame,
        repaint: LineRepaint,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) -> Option<()> {
        if repaint.screen_row >= repaint.content_height {
            return Some(());
        }
        let visible_rows = repaint
            .content_height
            .saturating_sub(repaint.screen_row)
            .min(repaint.new_rows);
        let rendered = self.render_editor_line(
            &frame.text,
            repaint.change.line,
            repaint.cursor_line,
            repaint.change.new_start,
            repaint.change.new_end,
            visible_rows,
            decorations,
            line_decorations,
        );
        if rendered.len() != visible_rows {
            return None;
        }
        for (offset, row) in rendered.into_iter().enumerate() {
            self.cached_rows[repaint.screen_row + offset] = Arc::new(row);
        }
        Some(())
    }

    fn update_cached_editor_footer(&mut self, status: &str, prompt: Option<&str>) {
        if !status.is_empty() || prompt.is_some() {
            let label = prompt.unwrap_or(status);
            let style = if prompt.is_some() {
                CellStyle::default()
            } else {
                CellStyle {
                    bold: true,
                    reverse: true,
                    ..CellStyle::default()
                }
            };
            let status_row = self.height.saturating_sub(1);
            self.cached_rows[status_row] =
                Arc::new(row_from_text(label, self.width, style, self.tab_width));
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
        let gutter = if self.line_numbers {
            self.number_width.min(self.width.saturating_sub(1))
        } else {
            0
        };
        let mut builder = GridBuilder::new(
            self.width.saturating_sub(gutter),
            height.max(1),
            self.tab_width,
            start,
            logical_line,
        );
        let visible = text.slice(start..end);
        builder.push_grapheme_document(&visible, usize::MAX, decorations);
        let mut rows = builder.rows;
        if gutter > 0 {
            prepend_line_numbers(
                &mut rows,
                &builder.row_lines,
                cursor_line,
                gutter,
                self.relative_numbers,
                line_decorations,
            );
        }
        if let Some(column) = self.color_column {
            mark_color_column(
                &mut rows,
                gutter.saturating_add(column.saturating_sub(1)),
                self.width,
                self.theme.mantle,
            );
        }
        ensure_row_backgrounds(&mut rows);
        apply_theme_to_rows(&mut rows, self.theme);
        rows
    }

    fn reuse_editor_render(
        &mut self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
    ) -> DesiredGrid {
        let reserved_rows = usize::from(!status.is_empty() || prompt.is_some());
        let content_height = self.height.saturating_sub(reserved_rows).max(1);
        let gutter = if self.line_numbers {
            self.number_width.min(self.width.saturating_sub(1))
        } else {
            0
        };
        let cursor_byte = frame.cursor_byte.min(frame.text.len());
        let cursor_line = frame.text.line_of_byte(cursor_byte);
        let line_start = frame.text.byte_of_line(cursor_line);
        let visible = frame.text.slice(line_start..cursor_byte.max(line_start));
        let (column, wrapped_row) = cursor_visual_position(
            &visible,
            0,
            visible.len(),
            self.width.saturating_sub(gutter).max(1),
            content_height,
            self.tab_width,
        );
        let base_row = self
            .cached_editor_render
            .as_ref()
            .and_then(|cached| {
                cached
                    .content_row_lines
                    .iter()
                    .position(|line| *line == Some(cursor_line))
            })
            .unwrap_or(0);
        let mut cursor = (
            column,
            base_row
                .saturating_add(wrapped_row)
                .min(content_height.saturating_sub(1)),
        );
        cursor.0 = cursor.0.saturating_add(gutter);
        if let Some(label) = prompt {
            let input = label.split("  │  ").next().unwrap_or(label);
            cursor = (
                display_width(input, self.tab_width).min(self.width.saturating_sub(1)),
                self.height.saturating_sub(1),
            );
        }
        self.epoch = self.epoch.saturating_add(1);
        DesiredGrid {
            epoch: self.epoch,
            width: self.width,
            height: self.height,
            rows: self.cached_rows.clone(),
            cursor,
        }
    }

    #[must_use]
    pub fn desired_workspace_grid(
        &mut self,
        model: &ClientViewModel,
        frames: &[(BufferId, EngineFrame)],
        status: &str,
        prompt: Option<&str>,
    ) -> DesiredGrid {
        self.desired_workspace_grid_with_decorations(model, frames, &[], status, prompt)
    }

    #[must_use]
    pub fn desired_workspace_grid_with_decorations(
        &mut self,
        model: &ClientViewModel,
        frames: &[(BufferId, EngineFrame)],
        decorations: &[(BufferId, Vec<DecorationSpan>)],
        status: &str,
        prompt: Option<&str>,
    ) -> DesiredGrid {
        self.desired_workspace_grid_with_line_decorations(
            model,
            frames,
            decorations,
            &[],
            status,
            prompt,
        )
    }

    #[must_use]
    pub fn desired_workspace_grid_with_line_decorations(
        &mut self,
        model: &ClientViewModel,
        frames: &[(BufferId, EngineFrame)],
        decorations: &[(BufferId, Vec<DecorationSpan>)],
        line_decorations: &[(BufferId, Vec<LineDecoration>)],
        status: &str,
        prompt: Option<&str>,
    ) -> DesiredGrid {
        let decorations = decorations
            .iter()
            .map(|(buffer_id, spans)| (*buffer_id, Arc::new(spans.clone())))
            .collect::<Vec<_>>();
        self.desired_workspace_grid_with_shared_decorations(
            model,
            frames,
            &decorations,
            line_decorations,
            status,
            prompt,
        )
    }

    #[must_use]
    pub fn desired_workspace_grid_with_shared_decorations(
        &mut self,
        model: &ClientViewModel,
        frames: &[(BufferId, EngineFrame)],
        decorations: &[(BufferId, SharedDecorations)],
        line_decorations: &[(BufferId, Vec<LineDecoration>)],
        status: &str,
        prompt: Option<&str>,
    ) -> DesiredGrid {
        let reserved_rows = usize::from(!status.is_empty() || prompt.is_some());
        let content_height = self.height.saturating_sub(reserved_rows).max(1);
        let tab = model.active_tab();
        let panes = self.workspace_panes(&tab.root, content_height);
        if let Some((window_id, rectangle)) = self.full_workspace_pane(&panes, content_height)
            && let Some(source) =
                workspace_pane_source(model, frames, decorations, line_decorations, window_id)
        {
            let grid = self.render_workspace_pane(
                window_id,
                rectangle,
                source.window.top_line,
                source.frame,
                source.decorations,
                source.line_decorations,
            );
            return self.finish_single_pane_workspace(grid, status, prompt);
        }

        let (mut rows, mut cursor) = self.compose_workspace_panes(
            model,
            frames,
            decorations,
            line_decorations,
            &panes,
            tab.active_window,
        );
        draw_split_borders(&tab.root, 0, 0, self.width, content_height, &mut rows);
        self.paint_workspace_footer(&mut rows, &mut cursor, status, prompt);
        self.finish_workspace_grid(rows, cursor)
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

    fn prepare_editor_realtime_updates(
        &mut self,
        frame: &EngineFrame,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) {
        if self.realtime_updates_prepared || self.cached_editor_render.is_none() {
            return;
        }
        self.realtime_updates_prepared = true;

        let byte = frame.cursor_byte.min(frame.text.len());
        if let Ok(transaction) =
            Transaction::new(DocumentRevision::new(0), vec![Edit::new(byte..byte, "x")])
            && let Ok(text) = frame.text.edited(&transaction)
        {
            let mapped = decorations
                .iter()
                .filter_map(|span| {
                    let start = transaction.map_offset(span.range.start, Bias::Left).ok()?;
                    let end = transaction.map_offset(span.range.end, Bias::Right).ok()?;
                    (start < end).then_some(DecorationSpan {
                        range: start..end,
                        style: span.style,
                        priority: span.priority,
                    })
                })
                .collect::<Vec<_>>();
            let mut preview = self.clone();
            let grid = preview.desired_editor_grid_with_line_decorations(
                &EngineFrame {
                    text,
                    cursor_byte: byte.saturating_add(1),
                },
                "",
                None,
                &mapped,
                line_decorations,
            );
            std::hint::black_box(grid);
        }

        let amount = self.height.saturating_sub(1).checked_div(2).unwrap_or(1);
        let target_line = frame
            .text
            .line_of_byte(frame.cursor_byte)
            .saturating_add(amount);
        if target_line <= frame.text.line_of_byte(frame.text.len()) {
            let mut preview = self.clone();
            preview.top_line = preview.top_line.saturating_add(amount);
            let moved = EngineFrame {
                text: frame.text.clone(),
                cursor_byte: frame.text.byte_of_line(target_line),
            };
            let grid = preview.desired_editor_grid_with_line_decorations(
                &moved,
                "",
                None,
                decorations,
                line_decorations,
            );
            std::hint::black_box(grid);
            preview.top_line = self.top_line;
            let grid = preview.desired_editor_grid_with_line_decorations(
                frame,
                "",
                None,
                decorations,
                line_decorations,
            );
            std::hint::black_box(grid);
        }
    }

    fn prefetch_editor_viewport(
        &mut self,
        frame: &EngineFrame,
        top_line: usize,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) {
        let already_cached = self.cached_editor_render.as_ref().is_some_and(|cached| {
            cached.top_line == top_line && cached.text.same_snapshot(&frame.text)
        }) || self.cached_editor_viewports.iter().any(|viewport| {
            viewport.render.top_line == top_line && viewport.render.text.same_snapshot(&frame.text)
        });
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
        let _ = preview.desired_editor_grid_with_line_decorations(
            frame,
            "",
            None,
            decorations,
            line_decorations,
        );
        preview.prepare_editor_realtime_updates(frame, decorations, line_decorations);
        if let Some(render) = preview.cached_editor_render
            && preview.cached_rows.len() == self.height
        {
            self.cached_editor_viewports.push(CachedEditorViewport {
                render,
                rows: preview.cached_rows,
            });
            self.prune_recent_editor_viewports();
        }
    }

    fn workspace_panes(
        &mut self,
        root: &SplitTree,
        content_height: usize,
    ) -> Vec<(WindowId, Rect)> {
        let mut panes = Vec::new();
        split_rectangles(
            root,
            Rect {
                column: 0,
                row: 0,
                width: self.width,
                height: content_height,
            },
            &mut panes,
        );
        self.pane_layouts
            .retain(|window_id, _| panes.iter().any(|(visible, _)| visible == window_id));
        panes
    }

    fn full_workspace_pane(
        &self,
        panes: &[(WindowId, Rect)],
        content_height: usize,
    ) -> Option<(WindowId, Rect)> {
        let [pane] = panes else {
            return None;
        };
        (pane.1
            == Rect {
                column: 0,
                row: 0,
                width: self.width,
                height: content_height,
            })
        .then_some(*pane)
    }

    fn compose_workspace_panes(
        &mut self,
        model: &ClientViewModel,
        frames: &[(BufferId, EngineFrame)],
        decorations: &[(BufferId, SharedDecorations)],
        line_decorations: &[(BufferId, Vec<LineDecoration>)],
        panes: &[(WindowId, Rect)],
        active_window: WindowId,
    ) -> (Vec<CellRow>, (usize, usize)) {
        let mut rows = vec![CellRow::default(); self.height];
        let mut cursor = (0, 0);
        for &(window_id, rectangle) in panes {
            let Some(source) =
                workspace_pane_source(model, frames, decorations, line_decorations, window_id)
            else {
                continue;
            };
            let grid = self.render_workspace_pane(
                window_id,
                rectangle,
                source.window.top_line,
                source.frame,
                source.decorations,
                source.line_decorations,
            );
            merge_pane_rows(&mut rows, rectangle, &grid.rows);
            if window_id == active_window {
                cursor = (
                    rectangle.column + grid.cursor.0,
                    rectangle.row + grid.cursor.1,
                );
            }
        }
        (rows, cursor)
    }

    fn paint_workspace_footer(
        &self,
        rows: &mut [CellRow],
        cursor: &mut (usize, usize),
        status: &str,
        prompt: Option<&str>,
    ) {
        let Some(label) = prompt.or((!status.is_empty()).then_some(status)) else {
            return;
        };
        let status_row = self.height.saturating_sub(1);
        rows[status_row] = row_from_text(
            label,
            self.width,
            prompt_style(prompt.is_some()),
            self.tab_width,
        );
        if prompt.is_some() {
            *cursor = self.prompt_cursor(label);
        }
    }

    fn finish_workspace_grid(
        &mut self,
        mut rows: Vec<CellRow>,
        cursor: (usize, usize),
    ) -> DesiredGrid {
        ensure_row_backgrounds(&mut rows);
        apply_theme_to_rows(&mut rows, self.theme);
        let rows: Vec<_> = rows
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                self.cached_rows
                    .get(index)
                    .filter(|cached| cached.as_ref() == &row)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(row))
            })
            .collect();
        self.cached_rows.clone_from(&rows);
        self.epoch = self.epoch.saturating_add(1);
        DesiredGrid {
            epoch: self.epoch,
            width: self.width,
            height: self.height,
            rows,
            cursor,
        }
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
        let pane = self.pane_layouts.entry(window_id).or_insert_with(|| {
            Box::new(Self::new(rectangle.width.max(1), rectangle.height.max(1)))
        });
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
        pane.desired_editor_grid_with_shared_decorations(
            frame,
            "",
            None,
            decorations,
            line_decorations,
        )
    }

    fn finish_single_pane_workspace(
        &mut self,
        grid: DesiredGrid,
        status: &str,
        prompt: Option<&str>,
    ) -> DesiredGrid {
        let mut rows = grid.rows;
        let mut cursor = grid.cursor;
        if !status.is_empty() || prompt.is_some() {
            let label = prompt.unwrap_or(status);
            let style = if prompt.is_some() {
                CellStyle::default()
            } else {
                CellStyle {
                    bold: true,
                    reverse: true,
                    ..CellStyle::default()
                }
            };
            let mut status_rows = vec![row_from_text(label, self.width, style, self.tab_width)];
            ensure_row_backgrounds(&mut status_rows);
            apply_theme_to_rows(&mut status_rows, self.theme);
            let row = status_rows.pop().unwrap_or_default();
            rows.push(
                self.cached_rows
                    .last()
                    .filter(|cached| cached.as_ref() == &row)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(row)),
            );
            if prompt.is_some() {
                let input = label.split("  │  ").next().unwrap_or(label);
                cursor = (
                    display_width(input, self.tab_width).min(self.width.saturating_sub(1)),
                    self.height.saturating_sub(1),
                );
            }
        }
        rows.resize_with(self.height, || Arc::new(CellRow::default()));
        rows.truncate(self.height);
        self.cached_rows.clone_from(&rows);
        self.epoch = self.epoch.saturating_add(1);
        DesiredGrid {
            epoch: self.epoch,
            width: self.width,
            height: self.height,
            rows,
            cursor,
        }
    }

    /// Draws Telescope's horizontal results/preview layout as a floating surface.
    /// The overlay deliberately lives above the editor grid so picker interaction
    /// never mutates buffer or split state.
    #[must_use]
    pub fn apply_picker_overlay(
        &mut self,
        grid: DesiredGrid,
        picker: &PickerOverlay,
    ) -> DesiredGrid {
        if self.width < 20 || self.height < 8 {
            return grid;
        }
        let mut rows = overlay_rows(&grid);
        let geometry = self.picker_geometry();
        let styles = self.picker_styles();
        self.draw_picker_shell(&mut rows, picker, geometry, styles);
        self.draw_picker_results(&mut rows, picker, geometry, styles);
        self.draw_picker_preview(&mut rows, picker, geometry, styles);
        self.finish_overlay(
            grid,
            rows,
            (
                geometry
                    .column
                    .saturating_add(3)
                    .saturating_add(display_width(&picker.prompt, self.tab_width))
                    .min(geometry.column + geometry.width - 2),
                geometry.prompt_row,
            ),
        )
    }

    fn picker_geometry(&self) -> PickerGeometry {
        let width = self.width.saturating_sub(4).clamp(16, 120);
        let height = self
            .height
            .saturating_sub(2)
            .min(self.height.saturating_mul(4) / 5)
            .max(8);
        let column = self.width.saturating_sub(width) / 2;
        let row = self.height.saturating_sub(height) / 2;
        let preview_visible = width >= 64;
        let inner_width = width.saturating_sub(2);
        let result_width = if preview_visible {
            inner_width.saturating_mul(2) / 5
        } else {
            inner_width
        };
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

    fn picker_styles(&self) -> PickerStyles {
        PickerStyles {
            base: CellStyle {
                foreground: Some(CellColor::Rgb(self.theme.text)),
                background: Some(CellColor::Rgb(self.theme.mantle)),
                ..CellStyle::default()
            },
            border: CellStyle {
                bold: true,
                foreground: Some(CellColor::Rgb(self.theme.blue)),
                background: Some(CellColor::Rgb(self.theme.mantle)),
                ..CellStyle::default()
            },
            title: CellStyle {
                bold: true,
                foreground: Some(CellColor::Rgb(self.theme.lavender)),
                background: Some(CellColor::Rgb(self.theme.mantle)),
                ..CellStyle::default()
            },
            selected: CellStyle {
                bold: true,
                foreground: Some(CellColor::Rgb(self.theme.text)),
                background: Some(CellColor::Rgb(self.theme.surface2)),
                ..CellStyle::default()
            },
            detail: CellStyle {
                foreground: Some(CellColor::Rgb(self.theme.subtext0)),
                background: Some(CellColor::Rgb(self.theme.mantle)),
                ..CellStyle::default()
            },
        }
    }

    fn draw_picker_shell(
        &self,
        rows: &mut [CellRow],
        picker: &PickerOverlay,
        geometry: PickerGeometry,
        styles: PickerStyles,
    ) {
        for target_row in geometry.row..geometry.row.saturating_add(geometry.height).min(rows.len())
        {
            paint_text(
                &mut rows[target_row],
                geometry.column,
                geometry.width,
                "",
                styles.base,
            );
        }
        let top = format!("╭{}╮", "─".repeat(geometry.width.saturating_sub(2)));
        let bottom = format!("╰{}╯", "─".repeat(geometry.width.saturating_sub(2)));
        paint_text(
            &mut rows[geometry.row],
            geometry.column,
            geometry.width,
            &top,
            styles.border,
        );
        paint_text(
            &mut rows[geometry.row + geometry.height - 1],
            geometry.column,
            geometry.width,
            &bottom,
            styles.border,
        );
        for target in rows
            .iter_mut()
            .take(geometry.row + geometry.height - 1)
            .skip(geometry.row + 1)
        {
            paint_text(target, geometry.column, 1, "│", styles.border);
            paint_text(
                target,
                geometry.column + geometry.width - 1,
                1,
                "│",
                styles.border,
            );
        }
        let title = format!(" {} ({}) ", picker.title, picker.rows.len());
        paint_text(
            &mut rows[geometry.row],
            geometry.column + 2,
            title.len().min(geometry.width.saturating_sub(4)),
            &title,
            styles.title,
        );
        let prompt = format!("❯ {}", picker.prompt);
        paint_text(
            &mut rows[geometry.prompt_row],
            geometry.column + 1,
            geometry.inner_width,
            &prompt,
            CellStyle {
                foreground: Some(CellColor::Rgb(self.theme.text)),
                background: Some(CellColor::Rgb(self.theme.surface0)),
                ..CellStyle::default()
            },
        );
        let footer_width =
            display_width(&picker.footer, 1).min(geometry.result_width.saturating_sub(2));
        if footer_width > 0 {
            paint_text(
                &mut rows[geometry.row + 1],
                geometry.column + 2,
                footer_width,
                &picker.footer,
                styles.detail,
            );
        }
    }

    fn draw_picker_results(
        &self,
        rows: &mut [CellRow],
        picker: &PickerOverlay,
        geometry: PickerGeometry,
        styles: PickerStyles,
    ) {
        let selected = picker.selected.min(picker.rows.len().saturating_sub(1));
        let result_start = selected
            .saturating_sub(geometry.content_height.saturating_sub(1) / 2)
            .min(picker.rows.len().saturating_sub(geometry.content_height));
        for (screen_index, item) in picker
            .rows
            .iter()
            .skip(result_start)
            .take(geometry.content_height)
            .enumerate()
        {
            let item_index = result_start + screen_index;
            let marker = if item_index == selected { "❯ " } else { "  " };
            let label = if item.detail.is_empty() {
                format!("{marker}{}", item.label)
            } else {
                format!("{marker}{}  {}", item.label, item.detail)
            };
            paint_text(
                &mut rows[geometry.content_start + screen_index],
                geometry.column + 1,
                geometry.result_width,
                &label,
                if item_index == selected {
                    styles.selected
                } else {
                    styles.base
                },
            );
        }
    }

    fn draw_picker_preview(
        &self,
        rows: &mut [CellRow],
        picker: &PickerOverlay,
        geometry: PickerGeometry,
        styles: PickerStyles,
    ) {
        if !geometry.preview_visible {
            return;
        }
        let divider_column = geometry.column + 1 + geometry.result_width;
        for target in rows
            .iter_mut()
            .take(geometry.prompt_row)
            .skip(geometry.row + 1)
        {
            paint_text(target, divider_column, 1, "│", styles.border);
        }
        let preview_column = divider_column + 1;
        paint_text(
            &mut rows[geometry.row + 1],
            preview_column,
            geometry.preview_width,
            &format!(" {} ", picker.preview_title),
            styles.title,
        );
        let preview_lines =
            wrap_popup_text(&picker.preview, geometry.preview_width, self.tab_width);
        for (line_offset, line) in preview_lines
            .iter()
            .filter(|line| line.source_line >= picker.preview_scroll)
            .take(geometry.content_height)
            .enumerate()
        {
            let style = if picker.preview_highlight_line == Some(line.source_line) {
                styles.selected
            } else {
                styles.base
            };
            paint_text(
                &mut rows[geometry.content_start + line_offset],
                preview_column,
                geometry.preview_width,
                &line.text,
                style,
            );
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

    /// Draws an nvim-cmp compatible popup menu at the active cursor and a
    /// documentation window alongside it when the selected item has docs.
    #[must_use]
    pub fn apply_completion_overlay(
        &mut self,
        grid: DesiredGrid,
        completion: &CompletionOverlay,
    ) -> DesiredGrid {
        if completion.rows.is_empty() || self.width < 12 || self.height < 4 {
            return grid;
        }
        let cursor = grid.cursor;
        let mut rows = overlay_rows(&grid);
        let menu = self.completion_menu(cursor, completion);
        let styles = self.completion_styles();
        self.draw_completion_menu(&mut rows, completion, menu, styles);
        self.draw_completion_documentation(
            &mut rows,
            completion,
            menu.rectangle,
            styles.base,
            styles.border,
        );
        self.finish_overlay(grid, rows, cursor)
    }

    fn completion_menu(
        &self,
        cursor: (usize, usize),
        completion: &CompletionOverlay,
    ) -> CompletionMenu {
        let visible_rows = completion.rows.len().min(10);
        let menu_height = visible_rows + 2;
        let longest = completion
            .rows
            .iter()
            .map(|row| {
                display_width(&row.label, 1)
                    .saturating_add(display_width(&row.detail, 1))
                    .saturating_add(display_width(&row.source, 1))
                    .saturating_add(7)
            })
            .max()
            .unwrap_or(20);
        let menu_width = longest.clamp(20, 52).min(self.width.saturating_sub(1));
        let menu_column = cursor.0.min(self.width.saturating_sub(menu_width));
        let menu_row = if cursor.1 + menu_height + 1 < self.height {
            cursor.1 + 1
        } else {
            cursor.1.saturating_sub(menu_height)
        };
        let selected = completion
            .selected
            .unwrap_or(0)
            .min(completion.rows.len().saturating_sub(1));
        let start = selected
            .saturating_sub(visible_rows.saturating_sub(1) / 2)
            .min(completion.rows.len().saturating_sub(visible_rows));
        CompletionMenu {
            rectangle: Rect {
                column: menu_column,
                row: menu_row,
                width: menu_width,
                height: menu_height,
            },
            visible_rows,
            start,
        }
    }

    fn completion_styles(&self) -> CompletionStyles {
        CompletionStyles {
            base: CellStyle {
                foreground: Some(CellColor::Rgb(self.theme.text)),
                background: Some(CellColor::Rgb(self.theme.surface0)),
                ..CellStyle::default()
            },
            border: CellStyle {
                foreground: Some(CellColor::Rgb(self.theme.blue)),
                background: Some(CellColor::Rgb(self.theme.base)),
                ..CellStyle::default()
            },
            selected: CellStyle {
                bold: true,
                foreground: Some(CellColor::Rgb(self.theme.text)),
                background: Some(CellColor::Rgb(self.theme.surface2)),
                ..CellStyle::default()
            },
        }
    }

    fn draw_completion_menu(
        &self,
        rows: &mut [CellRow],
        completion: &CompletionOverlay,
        menu: CompletionMenu,
        styles: CompletionStyles,
    ) {
        draw_popup_frame(
            rows,
            menu.rectangle.column,
            menu.rectangle.row,
            menu.rectangle.width,
            menu.rectangle.height,
            styles.base,
            styles.border,
            "completion",
        );
        for (offset, item) in completion
            .rows
            .iter()
            .skip(menu.start)
            .take(menu.visible_rows)
            .enumerate()
        {
            let item_index = menu.start + offset;
            let is_selected = completion.selected == Some(item_index);
            paint_text(
                &mut rows[menu.rectangle.row + 1 + offset],
                menu.rectangle.column + 1,
                menu.rectangle.width.saturating_sub(2),
                &completion_label(item, is_selected),
                if is_selected {
                    styles.selected
                } else {
                    styles.base
                },
            );
        }
    }

    fn draw_completion_documentation(
        &self,
        rows: &mut [CellRow],
        completion: &CompletionOverlay,
        menu: Rect,
        base_style: CellStyle,
        border_style: CellStyle,
    ) {
        if completion.documentation.is_empty() || self.width < 48 {
            return;
        }
        let available_right = self.width.saturating_sub(menu.column + menu.width + 1);
        let docs_width = 50
            .min(self.width.saturating_sub(2))
            .min(if available_right >= 24 {
                available_right
            } else {
                menu.column.saturating_sub(1)
            });
        if docs_width < 20 {
            return;
        }
        let docs_column = if available_right >= docs_width {
            menu.column + menu.width + 1
        } else {
            menu.column.saturating_sub(docs_width + 1)
        };
        let docs_height = (completion
            .documentation
            .lines()
            .skip(completion.documentation_scroll)
            .count()
            .min(12)
            + 2)
        .clamp(4, self.height.saturating_sub(menu.row).max(4));
        let docs_style = CellStyle {
            background: Some(CellColor::Rgb(self.theme.mantle)),
            ..base_style
        };
        draw_popup_frame(
            rows,
            docs_column,
            menu.row,
            docs_width,
            docs_height,
            docs_style,
            border_style,
            "documentation",
        );
        for (offset, line) in completion
            .documentation
            .lines()
            .skip(completion.documentation_scroll)
            .take(docs_height.saturating_sub(2))
            .enumerate()
        {
            paint_text(
                &mut rows[menu.row + 1 + offset],
                docs_column + 1,
                docs_width.saturating_sub(2),
                line,
                docs_style,
            );
        }
    }

    /// Rounded context float used by LSP hover/signature help, diagnostics,
    /// Git previews and other `open_floating_preview` surfaces.
    #[must_use]
    pub fn apply_text_popup(&mut self, grid: DesiredGrid, popup: &TextPopup) -> DesiredGrid {
        if popup.text.is_empty() || self.width < 8 || self.height < 4 {
            return grid;
        }
        let cursor = grid.cursor;
        let max_inner_width = popup_max_inner_width(self.width);
        let lines = wrap_popup_text(&popup.text, max_inner_width, self.tab_width);
        let inner_width = lines
            .iter()
            .map(|line| display_width(&line.text, self.tab_width))
            .max()
            .unwrap_or(1)
            .clamp(1, max_inner_width);
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
        let base_style = CellStyle {
            foreground: Some(CellColor::Rgb(self.theme.text)),
            background: Some(CellColor::Rgb(self.theme.mantle)),
            ..CellStyle::default()
        };
        let border_style = CellStyle {
            foreground: Some(CellColor::Rgb(self.theme.blue)),
            background: Some(CellColor::Rgb(self.theme.base)),
            ..CellStyle::default()
        };
        // A popup can extend past the final explicit cell in a short editor
        // row. Preserve that row's implicit tail before painting the float;
        // otherwise the terminal diff uses the popup border as the style for
        // ClearToEndOfLine and floods everything to its right with that color.
        for target in rows.iter_mut().skip(row).take(height) {
            materialize_row_tail(target, grid.width);
        }
        draw_popup_frame(
            &mut rows,
            column,
            row,
            width,
            height,
            base_style,
            border_style,
            &popup.title,
        );
        for (offset, line) in lines.iter().skip(scroll).take(visible_lines).enumerate() {
            paint_text(
                &mut rows[row + 1 + offset],
                column + 1,
                inner_width,
                &line.text,
                base_style,
            );
            paint_decorated_popup_text(
                &mut rows[row + 1 + offset],
                column + 1,
                line,
                &popup.decorations,
                base_style,
                self.tab_width,
            );
        }
        let cursor = popup_cursor.map_or(cursor, |(cursor_row, cursor_column)| {
            (
                column.saturating_add(1).saturating_add(cursor_column),
                row.saturating_add(1).saturating_add(cursor_row - scroll),
            )
        });
        self.finish_overlay(grid, rows, cursor)
    }

    /// The default nvim-dap-ui layout: scopes/breakpoints/stacks/watches on
    /// the left and REPL/console panels across the bottom.
    #[must_use]
    pub fn apply_debug_overlay(&mut self, grid: DesiredGrid, debug: &DebugOverlay) -> DesiredGrid {
        if self.width < 30 || self.height < 12 {
            return grid;
        }
        let mut rows = overlay_rows(&grid);
        let status_rows = 1;
        let usable_height = self.height.saturating_sub(status_rows);
        let bottom_height = usable_height.clamp(6, 10);
        let upper_height = usable_height.saturating_sub(bottom_height).max(4);
        let left_width = self
            .width
            .saturating_mul(3)
            .checked_div(10)
            .unwrap_or(30)
            .clamp(24, 40);
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
        let base_style = CellStyle {
            foreground: Some(CellColor::Rgb(self.theme.text)),
            background: Some(CellColor::Rgb(self.theme.mantle)),
            ..CellStyle::default()
        };
        let border_style = CellStyle {
            foreground: Some(CellColor::Rgb(self.theme.blue)),
            background: Some(CellColor::Rgb(self.theme.base)),
            ..CellStyle::default()
        };
        let panel_height = (upper_height / 4).max(3);
        for (index, (title, text)) in [
            ("Scopes", debug.scopes.as_ref()),
            ("Breakpoints", debug.breakpoints.as_ref()),
            ("Stacks", debug.stacks.as_ref()),
            ("Watches", debug.watches.as_ref()),
        ]
        .into_iter()
        .enumerate()
        {
            let row = index.saturating_mul(panel_height);
            let height = if index == 3 {
                upper_height.saturating_sub(row).max(3)
            } else {
                panel_height
            };
            draw_text_panel(
                &mut rows,
                0,
                row,
                left_width,
                height,
                title,
                text,
                base_style,
                border_style,
            );
        }
        let right_width = self.width.saturating_sub(left_width);
        let repl_width = right_width / 2;
        draw_text_panel(
            &mut rows,
            left_width,
            upper_height,
            repl_width,
            bottom_height,
            "REPL",
            &debug.repl,
            base_style,
            border_style,
        );
        draw_text_panel(
            &mut rows,
            left_width + repl_width,
            upper_height,
            right_width.saturating_sub(repl_width),
            bottom_height,
            "Console",
            &debug.console,
            base_style,
            border_style,
        );
        self.finish_overlay(grid, rows, cursor)
    }

    /// Paints AceJump-style labels directly over visible matches in the active
    /// pane using the same split, gutter, tab, and wrap geometry as the buffer.
    #[must_use]
    pub fn apply_ace_jump_overlay(
        &mut self,
        grid: DesiredGrid,
        model: &ClientViewModel,
        frame: &EngineFrame,
        overlay: &AceJumpOverlay,
    ) -> DesiredGrid {
        if overlay.targets.is_empty() {
            return grid;
        }
        let Some(pane) = self.ace_jump_pane(model, frame) else {
            return grid;
        };
        let positions = self.ace_jump_positions(frame, overlay, pane);
        let mut rows = overlay_rows(&grid);
        self.paint_ace_jump_positions(&mut rows, pane, &positions);
        let cursor = grid.cursor;
        self.finish_overlay(grid, rows, cursor)
    }

    fn ace_jump_pane(&self, model: &ClientViewModel, frame: &EngineFrame) -> Option<AceJumpPane> {
        let tab = model.active_tab();
        let mut panes = Vec::new();
        split_rectangles(
            &tab.root,
            Rect {
                column: 0,
                row: 0,
                width: self.width,
                height: self.height.saturating_sub(1).max(1),
            },
            &mut panes,
        );
        let (_, rectangle) = panes
            .into_iter()
            .find(|(window_id, _)| *window_id == tab.active_window)?;
        let window = model
            .windows
            .iter()
            .find(|window| window.id == tab.active_window)?;
        let gutter = if self.line_numbers {
            self.number_width.min(rectangle.width.saturating_sub(1))
        } else {
            0
        };
        let content_width = rectangle.width.saturating_sub(gutter).max(1);
        let start_byte = self.cached_frame_byte_of_line(frame, window.top_line);
        let end_byte = frame
            .text
            .byte_of_line(window.top_line.saturating_add(rectangle.height.max(1)));
        Some(AceJumpPane {
            rectangle,
            gutter,
            content_width,
            start_byte,
            end_byte,
        })
    }

    fn ace_jump_positions<'a>(
        &self,
        frame: &EngineFrame,
        overlay: &'a AceJumpOverlay,
        pane: AceJumpPane,
    ) -> Vec<AceJumpPosition<'a>> {
        let wanted = overlay
            .targets
            .iter()
            .filter(|target| target.byte >= pane.start_byte)
            .map(|target| (target.byte, target.label.as_ref()))
            .collect::<BTreeMap<_, _>>();
        let mut positions = Vec::new();
        let mut byte = pane.start_byte;
        let mut screen_row = 0_usize;
        let mut screen_column = 0_usize;
        let visible = frame.text.slice(pane.start_byte..pane.end_byte);
        for grapheme in visible.graphemes(true) {
            if let Some(label) = wanted.get(&byte)
                && screen_row < pane.rectangle.height
            {
                positions.push(AceJumpPosition {
                    row: screen_row,
                    column: screen_column,
                    label,
                });
            }
            advance_visual_position(
                grapheme,
                pane.content_width,
                self.tab_width,
                &mut screen_row,
                &mut screen_column,
            );
            byte = byte.saturating_add(grapheme.len());
            if screen_row >= pane.rectangle.height {
                break;
            }
        }
        positions
    }

    fn paint_ace_jump_positions(
        &self,
        rows: &mut [CellRow],
        pane: AceJumpPane,
        positions: &[AceJumpPosition<'_>],
    ) {
        let style = CellStyle {
            bold: true,
            foreground: Some(CellColor::Rgb(self.theme.base)),
            background: Some(CellColor::Rgb(self.theme.peach)),
            ..CellStyle::default()
        };
        for position in positions {
            let row = pane.rectangle.row.saturating_add(position.row);
            let column = pane
                .rectangle
                .column
                .saturating_add(pane.gutter)
                .saturating_add(position.column);
            let available = pane
                .rectangle
                .column
                .saturating_add(pane.rectangle.width)
                .saturating_sub(column);
            if let Some(target) = rows.get_mut(row) {
                paint_text(
                    target,
                    column,
                    display_width(position.label, self.tab_width).min(available),
                    position.label,
                    style,
                );
            }
        }
    }

    #[must_use]
    pub fn apply_status_overlay(
        &mut self,
        grid: DesiredGrid,
        status: &StatusOverlay,
    ) -> DesiredGrid {
        if self.height == 0 {
            return grid;
        }
        let width = grid.width;
        let height = grid.height;
        let cursor = grid.cursor;
        let mut rows = grid.rows;
        let row_index = self.height - 1;
        let row = self
            .cached_status_rows
            .iter()
            .rev()
            .find(|(cached, _)| cached == status)
            .map(|(_, row)| Arc::clone(row))
            .unwrap_or_else(|| {
                let base_style = CellStyle {
                    foreground: Some(CellColor::Rgb(self.theme.subtext1)),
                    background: Some(CellColor::Rgb(self.theme.mantle)),
                    ..CellStyle::default()
                };
                let mut row = CellRow {
                    cells: Vec::with_capacity(self.width),
                };
                let mut left_column = 0;
                for segment in &status.left {
                    let width =
                        display_width(&segment.text, 1).min(self.width.saturating_sub(left_column));
                    row.cells
                        .extend(row_from_text(&segment.text, width, segment.style, 1).cells);
                    left_column = left_column.saturating_add(width);
                }
                let right_width = status
                    .right
                    .iter()
                    .map(|segment| display_width(&segment.text, 1))
                    .sum::<usize>()
                    .min(self.width.saturating_sub(left_column));
                let mut right_column = self.width.saturating_sub(right_width);
                row.cells.extend((left_column..right_column).map(|_| Cell {
                    grapheme: single_byte_grapheme(b' '),
                    width: 1,
                    style: base_style,
                }));
                for segment in &status.right {
                    let width = display_width(&segment.text, 1)
                        .min(self.width.saturating_sub(right_column));
                    row.cells
                        .extend(row_from_text(&segment.text, width, segment.style, 1).cells);
                    right_column = right_column.saturating_add(width);
                }
                row.cells.extend((right_column..self.width).map(|_| Cell {
                    grapheme: single_byte_grapheme(b' '),
                    width: 1,
                    style: base_style,
                }));
                apply_theme_to_rows(std::slice::from_mut(&mut row), self.theme);
                let row = Arc::new(row);
                self.cached_status_rows
                    .push((status.clone(), Arc::clone(&row)));
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
        DesiredGrid {
            epoch: self.epoch,
            width,
            height,
            rows,
            cursor,
        }
    }

    fn finish_overlay(
        &mut self,
        grid: DesiredGrid,
        mut rows: Vec<CellRow>,
        cursor: (usize, usize),
    ) -> DesiredGrid {
        apply_theme_to_rows(&mut rows, self.theme);
        self.epoch = self.epoch.saturating_add(1);
        DesiredGrid {
            epoch: self.epoch,
            width: grid.width,
            height: grid.height,
            rows: rows.into_iter().map(Arc::new).collect(),
            cursor,
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

#[derive(Debug, Clone, Copy)]
struct PickerStyles {
    base: CellStyle,
    border: CellStyle,
    title: CellStyle,
    selected: CellStyle,
    detail: CellStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    column: usize,
    row: usize,
    width: usize,
    height: usize,
}

fn workspace_pane_source<'a>(
    model: &'a ClientViewModel,
    frames: &'a [(BufferId, EngineFrame)],
    decorations: &'a [(BufferId, SharedDecorations)],
    line_decorations: &'a [(BufferId, Vec<LineDecoration>)],
    window_id: WindowId,
) -> Option<WorkspacePaneSource<'a>> {
    let window = model.windows.iter().find(|window| window.id == window_id)?;
    let frame = frames
        .iter()
        .find(|(buffer_id, _)| *buffer_id == window.buffer_id)
        .map(|(_, frame)| frame)?;
    static EMPTY_DECORATIONS: std::sync::LazyLock<SharedDecorations> =
        std::sync::LazyLock::new(|| Arc::new(Vec::new()));
    let decorations = decorations
        .iter()
        .find(|(buffer_id, _)| *buffer_id == window.buffer_id)
        .map_or(&*EMPTY_DECORATIONS, |(_, spans)| spans);
    let line_decorations = line_decorations
        .iter()
        .find(|(buffer_id, _)| *buffer_id == window.buffer_id)
        .map_or(&[][..], |(_, spans)| spans.as_slice());
    Some(WorkspacePaneSource {
        window,
        frame,
        decorations,
        line_decorations,
    })
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
        PopupPlacement {
            column: right_of_cursor,
            row: below_cursor,
        },
        PopupPlacement {
            column: left_of_cursor,
            row: below_cursor,
        },
        PopupPlacement {
            column: right_of_cursor,
            row: above_cursor,
        },
        PopupPlacement {
            column: left_of_cursor,
            row: above_cursor,
        },
    ];

    (0..=bottom)
        .map(|row| PopupPlacement { column: right, row })
        .chain((0..=bottom).map(|row| PopupPlacement { column: 0, row }))
        .chain(cursor_local_candidates)
        .enumerate()
        .min_by_key(|(preference, placement)| {
            (
                occupied_cells_under_popup(grid, *placement, width, height),
                popup_contains_cursor(*placement, width, height, grid.cursor),
                *preference,
            )
        })
        .map_or(PopupPlacement { column: 0, row: 0 }, |(_, placement)| {
            placement
        })
}

fn popup_max_inner_width(terminal_width: usize) -> usize {
    terminal_width.saturating_sub(4).clamp(4, 80)
}

fn popup_contains_cursor(
    placement: PopupPlacement,
    width: usize,
    height: usize,
    cursor: (usize, usize),
) -> bool {
    cursor.0 >= placement.column
        && cursor.0 < placement.column.saturating_add(width)
        && cursor.1 >= placement.row
        && cursor.1 < placement.row.saturating_add(height)
}

fn occupied_cells_under_popup(
    grid: &DesiredGrid,
    placement: PopupPlacement,
    width: usize,
    height: usize,
) -> usize {
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
                    occupied = occupied.saturating_add(
                        cell_end
                            .min(popup_end)
                            .saturating_sub(cell_start.max(placement.column)),
                    );
                }
            }
            occupied
        })
        .sum()
}

fn apply_theme_to_rows(rows: &mut [CellRow], theme: CatppuccinPalette) {
    if theme == CatppuccinPalette::MOCHA {
        return;
    }
    for cell in rows.iter_mut().flat_map(|row| &mut row.cells) {
        cell.style.foreground = cell
            .style
            .foreground
            .map(|color| theme.remap_from_mocha(color));
        cell.style.background = cell
            .style
            .background
            .map(|color| theme.remap_from_mocha(color));
    }
}

fn ensure_row_backgrounds(rows: &mut [CellRow]) {
    for row in rows {
        if row.cells.is_empty() {
            row.cells.push(Cell {
                grapheme: " ".into(),
                width: 1,
                style: CellStyle::default(),
            });
        }
    }
}

fn paint_text(row: &mut CellRow, column: usize, width: usize, text: &str, style: CellStyle) {
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
    let before_width = before
        .iter()
        .map(|cell| usize::from(cell.width))
        .sum::<usize>();
    before.extend((before_width..column).map(|_| Cell {
        grapheme: " ".into(),
        width: 1,
        style,
    }));
    let mut replacement = row_from_text(text, width, style, 1);
    let replacement_width = replacement
        .cells
        .iter()
        .map(|cell| usize::from(cell.width))
        .sum::<usize>();
    replacement
        .cells
        .extend((replacement_width..width).map(|_| Cell {
            grapheme: " ".into(),
            width: 1,
            style,
        }));
    before.append(&mut replacement.cells);
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

#[allow(clippy::too_many_arguments)]
fn draw_popup_frame(
    rows: &mut [CellRow],
    column: usize,
    row: usize,
    width: usize,
    height: usize,
    base_style: CellStyle,
    border_style: CellStyle,
    title: &str,
) {
    if width < 2 || height < 2 || row.saturating_add(height) > rows.len() {
        return;
    }
    for target in rows.iter_mut().skip(row).take(height) {
        paint_text(target, column, width, "", base_style);
    }
    paint_text(
        &mut rows[row],
        column,
        width,
        &format!("╭{}╮", "─".repeat(width.saturating_sub(2))),
        border_style,
    );
    paint_text(
        &mut rows[row + height - 1],
        column,
        width,
        &format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
        border_style,
    );
    for target in rows.iter_mut().take(row + height - 1).skip(row + 1) {
        paint_text(target, column, 1, "│", border_style);
        paint_text(target, column + width - 1, 1, "│", border_style);
    }
    if !title.is_empty() {
        let title = format!(" {title} ");
        paint_text(
            &mut rows[row],
            column + 2,
            display_width(&title, 1).min(width.saturating_sub(4)),
            &title,
            CellStyle {
                bold: true,
                ..border_style
            },
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text_panel(
    rows: &mut [CellRow],
    column: usize,
    row: usize,
    width: usize,
    height: usize,
    title: &str,
    text: &str,
    base_style: CellStyle,
    border_style: CellStyle,
) {
    if width < 4 || height < 3 {
        return;
    }
    draw_popup_frame(
        rows,
        column,
        row,
        width,
        height,
        base_style,
        border_style,
        title,
    );
    for (offset, line) in text.lines().take(height.saturating_sub(2)).enumerate() {
        paint_text(
            &mut rows[row + 1 + offset],
            column + 1,
            width.saturating_sub(2),
            line,
            base_style,
        );
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
        let without_newline = source_with_newline
            .strip_suffix('\n')
            .unwrap_or(source_with_newline);
        let source_line = without_newline
            .strip_suffix('\r')
            .unwrap_or(without_newline);
        output.extend(wrap_popup_line(
            source_line,
            source_offset,
            source_line_number,
            width,
            tab_width,
        ));
        source_offset += source_with_newline.len();
    }
    if output.is_empty() {
        output.push(WrappedPopupLine {
            text: String::new(),
            source_start: 0,
            source_line: 0,
        });
    }
    output
}

fn wrap_popup_line(
    source: &str,
    source_offset: usize,
    source_line: usize,
    width: usize,
    tab_width: usize,
) -> Vec<WrappedPopupLine> {
    if source.is_empty() || display_width(source, tab_width) <= width {
        return vec![WrappedPopupLine {
            text: source.to_owned(),
            source_start: source_offset,
            source_line,
        }];
    }
    let mut output = Vec::new();
    let mut start = 0;
    while start < source.len() {
        let (display_end, next) = popup_wrap_boundary(source, start, width, tab_width);
        output.push(WrappedPopupLine {
            text: source[start..display_end].trim_end().to_owned(),
            source_start: source_offset + start,
            source_line,
        });
        start = skip_grapheme_whitespace(source, next);
    }
    output
}

fn popup_wrap_boundary(
    source: &str,
    start: usize,
    width: usize,
    tab_width: usize,
) -> (usize, usize) {
    let mut used = 0_usize;
    let mut end = start;
    let mut whitespace = None;
    for (relative, grapheme) in source[start..].grapheme_indices(true) {
        let grapheme_start = start + relative;
        let grapheme_end = grapheme_start + grapheme.len();
        let grapheme_width = if grapheme == "\t" {
            tab_width.max(1) - (used % tab_width.max(1))
        } else {
            display_width(grapheme, 1)
        };
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
    let end = if end == start {
        first_grapheme_end(source, start)
    } else {
        end
    };
    match whitespace {
        Some((space_start, space_end)) if space_start > start && end < source.len() => {
            (space_start, space_end)
        }
        _ => (end, end),
    }
}

fn first_grapheme_end(source: &str, start: usize) -> usize {
    source[start..]
        .graphemes(true)
        .next()
        .map_or(source.len(), |grapheme| start + grapheme.len())
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
            let width = if grapheme == "\t" {
                tab_width - ((display_column - column) % tab_width)
            } else {
                display_width(grapheme, 1)
            };
            apply_style_to_columns(
                row,
                display_column,
                display_column.saturating_add(width),
                merge_styles(base_style, overlay),
            );
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
        bold: base.bold || overlay.bold,
        italic: base.italic || overlay.italic,
        underline: base.underline || overlay.underline,
        strikethrough: base.strikethrough || overlay.strikethrough,
        reverse: base.reverse || overlay.reverse,
        foreground: if overlay.foreground.is_some() {
            overlay.foreground
        } else {
            base.foreground
        },
        background: if overlay.background.is_some() {
            overlay.background
        } else {
            base.background
        },
    }
}

fn ranges_overlap(left: Range<usize>, right: Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

fn split_rectangles(tree: &SplitTree, rectangle: Rect, panes: &mut Vec<(WindowId, Rect)>) {
    match tree {
        SplitTree::Window(window) => panes.push((*window, rectangle)),
        SplitTree::Split {
            axis,
            ratio_milli,
            first,
            second,
        } => match axis {
            SplitAxis::Vertical => {
                let available = rectangle.width.saturating_sub(1);
                let first_width = available
                    .saturating_mul(usize::from(*ratio_milli))
                    .checked_div(1000)
                    .unwrap_or(0)
                    .clamp(1, available.saturating_sub(1).max(1));
                split_rectangles(
                    first,
                    Rect {
                        width: first_width,
                        ..rectangle
                    },
                    panes,
                );
                split_rectangles(
                    second,
                    Rect {
                        column: rectangle.column + first_width + 1,
                        width: available.saturating_sub(first_width),
                        ..rectangle
                    },
                    panes,
                );
            }
            SplitAxis::Horizontal => {
                let available = rectangle.height.saturating_sub(1);
                let first_height = available
                    .saturating_mul(usize::from(*ratio_milli))
                    .checked_div(1000)
                    .unwrap_or(0)
                    .clamp(1, available.saturating_sub(1).max(1));
                split_rectangles(
                    first,
                    Rect {
                        height: first_height,
                        ..rectangle
                    },
                    panes,
                );
                split_rectangles(
                    second,
                    Rect {
                        row: rectangle.row + first_height + 1,
                        height: available.saturating_sub(first_height),
                        ..rectangle
                    },
                    panes,
                );
            }
        },
    }
}

fn pad_row_to(row: &mut CellRow, column: usize) {
    let width: usize = row.cells.iter().map(|cell| usize::from(cell.width)).sum();
    for _ in width..column {
        row.cells.push(Cell {
            grapheme: " ".into(),
            width: 1,
            style: CellStyle::default(),
        });
    }
}

fn materialize_row_tail(row: &mut CellRow, width: usize) {
    let current = row
        .cells
        .iter()
        .map(|cell| usize::from(cell.width))
        .sum::<usize>();
    let style = row
        .cells
        .last()
        .map_or_else(CellStyle::default, |cell| cell.style);
    row.cells.extend((current..width).map(|_| Cell {
        grapheme: " ".into(),
        width: 1,
        style,
    }));
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
            row.cells.push(Cell {
                grapheme: " ".into(),
                width: 1,
                style: CellStyle {
                    background: Some(CellColor::Rgb(color)),
                    ..CellStyle::default()
                },
            });
        }
    }
}

fn put_border(rows: &mut [CellRow], column: usize, row_index: usize, grapheme: &str) {
    let Some(row) = rows.get_mut(row_index) else {
        return;
    };
    let border = Cell {
        grapheme: grapheme.into(),
        width: 1,
        style: CellStyle {
            reverse: true,
            ..CellStyle::default()
        },
    };
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

fn draw_split_borders(
    tree: &SplitTree,
    column: usize,
    row: usize,
    width: usize,
    height: usize,
    rows: &mut [CellRow],
) {
    let SplitTree::Split {
        axis,
        ratio_milli,
        first,
        second,
    } = tree
    else {
        return;
    };
    match axis {
        SplitAxis::Vertical => {
            let available = width.saturating_sub(1);
            let first_width = available
                .saturating_mul(usize::from(*ratio_milli))
                .checked_div(1000)
                .unwrap_or(0)
                .clamp(1, available.saturating_sub(1).max(1));
            let border = column + first_width;
            for row_index in row..row.saturating_add(height) {
                put_border(rows, border, row_index, "│");
            }
            draw_split_borders(first, column, row, first_width, height, rows);
            draw_split_borders(
                second,
                border + 1,
                row,
                available.saturating_sub(first_width),
                height,
                rows,
            );
        }
        SplitAxis::Horizontal => {
            let available = height.saturating_sub(1);
            let first_height = available
                .saturating_mul(usize::from(*ratio_milli))
                .checked_div(1000)
                .unwrap_or(0)
                .clamp(1, available.saturating_sub(1).max(1));
            let border = row + first_height;
            for target_column in column..column.saturating_add(width) {
                put_border(rows, target_column, border, "─");
            }
            draw_split_borders(first, column, row, width, first_height, rows);
            draw_split_borders(
                second,
                column,
                border + 1,
                width,
                available.saturating_sub(first_height),
                rows,
            );
        }
    }
}

fn byte_at_visual_cell(
    text: &FrameText,
    document_start: usize,
    width: usize,
    tab_width: usize,
    target_column: usize,
    target_row: usize,
) -> Option<usize> {
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
        let displayed = if grapheme == "\t" {
            vec![" ".to_owned(); tab_width - (column % tab_width)]
        } else {
            escape_grapheme(grapheme)
        };
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
            if row == target_row
                && target_column >= column
                && target_column < column.saturating_add(cell_width)
            {
                return Some(grapheme_start);
            }
            column = column.saturating_add(cell_width);
            last_grapheme_on_row = Some(grapheme_start);
        }
    }
    (row <= target_row).then_some(last_grapheme_on_row.unwrap_or(absolute_byte))
}

fn advance_visual_position(
    grapheme: &str,
    width: usize,
    tab_width: usize,
    row: &mut usize,
    column: &mut usize,
) {
    if grapheme == "\n" {
        *row = row.saturating_add(1);
        *column = 0;
        return;
    }
    let cells = if grapheme == "\t" {
        vec![" ".to_owned(); tab_width - (*column % tab_width)]
    } else {
        escape_grapheme(grapheme)
    };
    for cell in cells {
        let cell_width = UnicodeWidthStr::width(cell.as_str()).max(1);
        if column.saturating_add(cell_width) > width && *column > 0 {
            *row = row.saturating_add(1);
            *column = 0;
        }
        *column = column.saturating_add(cell_width);
    }
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
    fn new(
        width: usize,
        height: usize,
        tab_width: usize,
        document_start: usize,
        start_line: usize,
    ) -> Self {
        let mut rows = Vec::with_capacity(height.max(1));
        rows.push(CellRow {
            cells: Vec::with_capacity(width.min(256)),
        });
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

    fn push_grapheme_document(
        &mut self,
        visible: &str,
        cursor_byte: usize,
        decorations: &[DecorationSpan],
    ) {
        if visible.is_ascii() {
            self.push_ascii_document(visible.as_bytes(), cursor_byte, decorations);
            return;
        }
        let mut absolute_byte = self.document_start;
        let mut decoration_resolver = DecorationResolver::new(decorations);
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

    fn push_ascii_document(
        &mut self,
        visible: &[u8],
        cursor_byte: usize,
        decorations: &[DecorationSpan],
    ) {
        let mut absolute_byte = self.document_start;
        let mut decoration_resolver = DecorationResolver::new(decorations);
        if cursor_byte < self.document_start {
            self.cursor = Some((0, 0));
        }
        for byte in visible.iter().copied() {
            self.mark_cursor(absolute_byte, cursor_byte);
            let style = decoration_resolver
                .style_or_default(absolute_byte..absolute_byte.saturating_add(1));
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
            _ if grapheme.chars().all(|character| !character.is_control()) => {
                self.push_cell(grapheme, style)
            }
            _ => escape_grapheme(grapheme)
                .iter()
                .all(|escaped| self.push_cell(escaped, style)),
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
            row.cells.push(Cell {
                grapheme: single_byte_grapheme(byte),
                width: 1,
                style,
            });
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
            row.cells.push(Cell {
                grapheme: grapheme.into(),
                width: u8::try_from(width).unwrap_or(u8::MAX),
                style,
            });
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
        self.rows.push(CellRow {
            cells: Vec::with_capacity(self.width.min(256)),
        });
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
    fn new(decorations: &'a [DecorationSpan]) -> Self {
        let starts = (!decorations
            .windows(2)
            .all(|pair| pair[0].range.start <= pair[1].range.start))
        .then(|| {
            let mut starts = (0..decorations.len()).collect::<Vec<_>>();
            starts.sort_by_key(|index| (decorations[*index].range.start, *index));
            starts
        });
        Self {
            decorations,
            starts,
            next: 0,
            active: Vec::new(),
            cached_style: CellStyle::default(),
            valid_until: 0,
        }
    }

    fn style(&mut self, range: Range<usize>) -> CellStyle {
        if range.end <= self.valid_until {
            return self.cached_style;
        }
        self.active
            .retain(|index| self.decorations[*index].range.end > range.start);
        while let Some(index) = self.next_decoration()
            && self.decorations[index].range.start < range.end
        {
            self.active.push(index);
            self.next += 1;
        }
        self.cached_style = decoration_style_from_indices(
            self.decorations,
            self.active.iter().copied(),
            range.clone(),
        );
        let next_start = self
            .next_decoration()
            .map_or(usize::MAX, |index| self.decorations[index].range.start);
        let active_end = self
            .active
            .iter()
            .map(|index| self.decorations[*index].range.end)
            .min()
            .unwrap_or(usize::MAX);
        self.valid_until = next_start.min(active_end).max(range.end);
        self.cached_style
    }

    fn style_or_default(&mut self, range: Range<usize>) -> CellStyle {
        if self.decorations.is_empty() {
            CellStyle::default()
        } else {
            self.style(range)
        }
    }

    fn next_decoration(&self) -> Option<usize> {
        self.starts.as_ref().map_or_else(
            || (self.next < self.decorations.len()).then_some(self.next),
            |starts| starts.get(self.next).copied(),
        )
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
        let prefix = line_number_prefix(
            row_lines.get(index).copied().flatten(),
            cursor_line,
            width,
            relative,
            line_decorations,
        );
        let prefix_len = prefix.cells.len();
        row.cells.reserve(prefix_len);
        row.cells.extend(prefix.cells);
        row.cells.rotate_right(prefix_len);
    }
}

fn replace_line_number(
    row: &mut CellRow,
    logical_line: usize,
    cursor_line: usize,
    width: usize,
    relative: bool,
    line_decorations: &[LineDecoration],
) {
    let prefix = line_number_prefix(
        Some(logical_line),
        cursor_line,
        width,
        relative,
        line_decorations,
    );
    for (target, replacement) in row.cells.iter_mut().zip(prefix.cells) {
        *target = replacement;
    }
}

fn line_number_prefix(
    logical_line: Option<usize>,
    cursor_line: usize,
    width: usize,
    relative: bool,
    line_decorations: &[LineDecoration],
) -> CellRow {
    let style = logical_line
        .and_then(|line| {
            line_decorations
                .iter()
                .rev()
                .find(|decoration| decoration.line == line)
        })
        .map_or(
            CellStyle {
                foreground: Some(CellColor::Rgb(CatppuccinPalette::MOCHA.overlay1)),
                ..CellStyle::default()
            },
            |decoration| decoration.style,
        );
    let mut cells = vec![
        Cell {
            grapheme: single_byte_grapheme(b' '),
            width: 1,
            style,
        };
        width
    ];
    if let Some(line) = logical_line {
        let mut number = if relative && line != cursor_line {
            line.abs_diff(cursor_line)
        } else {
            line.saturating_add(1)
        };
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

fn single_byte_grapheme(byte: u8) -> CellGrapheme {
    let mut bytes = [0; INLINE_GRAPHEME_BYTES];
    bytes[0] = byte;
    CellGrapheme::Inline { len: 1, bytes }
}

#[cfg(test)]
fn decoration_style(decorations: &[DecorationSpan], range: Range<usize>) -> CellStyle {
    decoration_style_from_indices(decorations, 0..decorations.len(), range)
}

fn decoration_style_from_indices(
    decorations: &[DecorationSpan],
    indices: impl IntoIterator<Item = usize>,
    range: Range<usize>,
) -> CellStyle {
    resolve_decoration_style(decorations, indices, range, CellStyle::default()).0
}

fn decoration_overlay_style(
    decorations: &[DecorationSpan],
    range: Range<usize>,
) -> Option<CellStyle> {
    let (style, overlaps) = resolve_decoration_style(
        decorations,
        0..decorations.len(),
        range,
        transparent_cell_style(),
    );
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
        style.bold |= decoration.style.bold;
        style.italic |= decoration.style.italic;
        style.underline |= decoration.style.underline;
        style.strikethrough |= decoration.style.strikethrough;
        style.reverse |= decoration.style.reverse;
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
    CellStyle {
        bold: false,
        italic: false,
        underline: false,
        strikethrough: false,
        reverse: false,
        foreground: None,
        background: None,
    }
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
    CellStyle {
        bold: true,
        reverse: true,
        ..CellStyle::default()
    }
}

fn completion_label(item: &CompletionOverlayRow, selected: bool) -> String {
    let marker = if selected { "❯" } else { " " };
    let detail = if item.detail.is_empty() {
        String::new()
    } else {
        format!(" · {}", item.detail)
    };
    format!("{marker} {}{detail} [{}]", item.label, item.source)
}

fn row_from_text(text: &str, width: usize, style: CellStyle, tab_width: usize) -> CellRow {
    if text.bytes().all(|byte| matches!(byte, 0x20..=0x7e)) {
        return CellRow {
            cells: text
                .bytes()
                .take(width)
                .map(|byte| Cell {
                    grapheme: single_byte_grapheme(byte),
                    width: 1,
                    style,
                })
                .collect(),
        };
    }
    let mut row = CellRow {
        cells: Vec::with_capacity(width.min(text.len())),
    };
    let mut column = 0;
    for grapheme in text.graphemes(true) {
        if grapheme != "\t" && grapheme.chars().all(|character| !character.is_control()) {
            let cell_width = UnicodeWidthStr::width(grapheme).max(1);
            if column + cell_width > width {
                return row;
            }
            row.cells.push(Cell {
                grapheme: grapheme.into(),
                width: u8::try_from(cell_width).unwrap_or(u8::MAX),
                style,
            });
            column += cell_width;
            continue;
        }
        let representations = if grapheme == "\t" {
            vec![" ".to_owned(); tab_width - (column % tab_width)]
        } else {
            escape_grapheme(grapheme)
        };
        for representation in representations {
            let cell_width = UnicodeWidthStr::width(representation.as_str()).max(1);
            if column + cell_width > width {
                return row;
            }
            row.cells.push(Cell {
                grapheme: representation.into(),
                width: u8::try_from(cell_width).unwrap_or(u8::MAX),
                style,
            });
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
            width += escape_grapheme(grapheme)
                .iter()
                .map(|value| UnicodeWidthStr::width(value.as_str()).max(1))
                .sum::<usize>();
        }
    }
    width
}

fn line_of_byte(text: &str, byte: usize) -> usize {
    text.get(..byte.min(text.len()))
        .unwrap_or_default()
        .bytes()
        .filter(|value| *value == b'\n')
        .count()
}

#[derive(Debug, Clone, Copy)]
struct SingleLineChange {
    line: usize,
    old_start: usize,
    old_end: usize,
    new_start: usize,
    new_end: usize,
}

#[derive(Clone, Copy)]
struct LineRepaint {
    change: SingleLineChange,
    cursor_line: usize,
    screen_row: usize,
    content_height: usize,
    new_rows: usize,
}

impl SingleLineChange {
    fn old_text(self, text: &FrameText) -> Cow<'_, str> {
        text.slice(self.old_start..self.old_end)
    }

    fn new_text(self, text: &FrameText) -> Cow<'_, str> {
        text.slice(self.new_start..self.new_end)
    }
}

fn single_line_change(old: &FrameText, new: &FrameText) -> Option<SingleLineChange> {
    if let Some(change) = new.single_line_change_from(old) {
        return Some(SingleLineChange {
            line: change.line,
            old_start: change.old_start,
            old_end: change.old_end,
            new_start: change.new_start,
            new_end: change.new_end,
        });
    }
    let mut difference = old
        .bytes()
        .zip(new.bytes())
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| old.len().min(new.len()));
    if old == new {
        return None;
    }
    while difference > 0 && (!old.is_char_boundary(difference) || !new.is_char_boundary(difference))
    {
        difference -= 1;
    }
    let old_start = old
        .get(..difference)?
        .rfind('\n')
        .map_or(0, |offset| offset + 1);
    let new_start = new
        .get(..difference)?
        .rfind('\n')
        .map_or(0, |offset| offset + 1);
    let old_end = old[difference..]
        .find('\n')
        .map_or(old.len(), |offset| difference + offset);
    let new_end = new[difference..]
        .find('\n')
        .map_or(new.len(), |offset| difference + offset);
    if old.get(..old_start) != new.get(..new_start) || old.get(old_end..) != new.get(new_end..) {
        return None;
    }
    Some(SingleLineChange {
        line: line_of_byte(old, old_start),
        old_start,
        old_end,
        new_start,
        new_end,
    })
}

fn decorations_match_outside_change(
    old: &[DecorationSpan],
    new: &[DecorationSpan],
    change: SingleLineChange,
) -> bool {
    let mut old_index = 0;
    let mut new_index = 0;
    while old_index < old.len() && new_index < new.len() {
        let old_span = &old[old_index];
        let new_span = &new[new_index];
        if old_span.range.end > change.old_start || new_span.range.end > change.new_start {
            break;
        }
        if old_span != new_span {
            return false;
        }
        old_index += 1;
        new_index += 1;
    }
    if old
        .get(old_index)
        .is_some_and(|span| span.range.end <= change.old_start)
        || new
            .get(new_index)
            .is_some_and(|span| span.range.end <= change.new_start)
    {
        return false;
    }

    let skip_changed = |spans: &[DecorationSpan], index: &mut usize, start, end| {
        while *index < spans.len() && spans[*index].range.start < end {
            let span = &spans[*index];
            if span.range.start < start || span.range.end > end {
                return false;
            }
            *index += 1;
        }
        true
    };
    if !skip_changed(old, &mut old_index, change.old_start, change.old_end)
        || !skip_changed(new, &mut new_index, change.new_start, change.new_end)
        || old.len().saturating_sub(old_index) != new.len().saturating_sub(new_index)
    {
        return false;
    }

    let old = &old[old_index..];
    let new = &new[new_index..];
    if change.new_end >= change.old_end {
        let delta = change.new_end - change.old_end;
        old.iter().zip(new).all(|(old, new)| {
            old.style == new.style
                && old.priority == new.priority
                && old.range.start.checked_add(delta) == Some(new.range.start)
                && old.range.end.checked_add(delta) == Some(new.range.end)
        })
    } else {
        let delta = change.old_end - change.new_end;
        old.iter().zip(new).all(|(old, new)| {
            old.style == new.style
                && old.priority == new.priority
                && old.range.start.checked_sub(delta) == Some(new.range.start)
                && old.range.end.checked_sub(delta) == Some(new.range.end)
        })
    }
}

fn retain_decorations(
    decorations: &[DecorationSpan],
    shared: Option<&SharedDecorations>,
) -> SharedDecorations {
    shared.map_or_else(|| Arc::new(decorations.to_vec()), Arc::clone)
}

fn decorations_match_range(
    left: &[DecorationSpan],
    right: &[DecorationSpan],
    range: &Range<usize>,
) -> bool {
    let overlaps =
        |span: &&DecorationSpan| span.range.start < range.end && range.start < span.range.end;
    left.iter()
        .filter(overlaps)
        .eq(right.iter().filter(overlaps))
}

fn visual_line_rows(text: &str, width: usize, tab_width: usize) -> usize {
    let mut row = 0;
    let mut column = 0;
    for grapheme in text.graphemes(true) {
        if grapheme == "\t" {
            let spaces = tab_width.max(1) - (column % tab_width.max(1));
            for _ in 0..spaces {
                advance_visual_cell(&mut row, &mut column, width, 1);
            }
        } else if grapheme.chars().all(|character| !character.is_control()) {
            advance_visual_cell(
                &mut row,
                &mut column,
                width,
                UnicodeWidthStr::width(grapheme).max(1),
            );
        } else {
            for escaped in escape_grapheme(grapheme) {
                advance_visual_cell(
                    &mut row,
                    &mut column,
                    width,
                    UnicodeWidthStr::width(escaped.as_str()).max(1),
                );
            }
        }
    }
    row.saturating_add(1)
}

fn cursor_visual_position(
    text: &str,
    start_byte: usize,
    cursor_byte: usize,
    width: usize,
    height: usize,
    tab_width: usize,
) -> (usize, usize) {
    if cursor_byte <= start_byte {
        return (0, 0);
    }
    let visible = text
        .get(start_byte..cursor_byte.min(text.len()))
        .unwrap_or_default();
    let mut row = 0_usize;
    let mut column = 0_usize;
    for grapheme in visible.graphemes(true) {
        if grapheme == "\n" {
            row = row.saturating_add(1);
            column = 0;
        } else if grapheme == "\t" {
            let spaces = tab_width.max(1) - (column % tab_width.max(1));
            for _ in 0..spaces {
                advance_visual_cell(&mut row, &mut column, width, 1);
            }
        } else if grapheme.chars().all(|character| !character.is_control()) {
            advance_visual_cell(
                &mut row,
                &mut column,
                width,
                UnicodeWidthStr::width(grapheme).max(1),
            );
        } else {
            for escaped in escape_grapheme(grapheme) {
                advance_visual_cell(
                    &mut row,
                    &mut column,
                    width,
                    UnicodeWidthStr::width(escaped.as_str()).max(1),
                );
            }
        }
        if row >= height {
            break;
        }
    }
    (
        column.min(width.saturating_sub(1)),
        row.min(height.saturating_sub(1)),
    )
}

fn advance_visual_cell(row: &mut usize, column: &mut usize, width: usize, cell_width: usize) {
    if column.saturating_add(cell_width) > width && *column > 0 {
        *row = row.saturating_add(1);
        *column = 0;
    }
    *column = column.saturating_add(cell_width);
}

#[must_use]
pub fn diff(previous: Option<&DesiredGrid>, desired: &DesiredGrid) -> Vec<TerminalPatch> {
    let mut patches = Vec::with_capacity(desired.height.saturating_mul(3).saturating_add(4));
    patches.push(TerminalPatch::ShowCursor(false));
    if previous.is_none_or(|old| old.width != desired.width || old.height != desired.height) {
        patches.push(TerminalPatch::Clear);
    }
    for row_index in 0..desired.height {
        let row = desired.rows.get(row_index).cloned().unwrap_or_default();
        let changed = previous.and_then(|old| old.rows.get(row_index)) != Some(&row);
        if changed {
            patches.push(TerminalPatch::MoveTo {
                column: 0,
                row: row_index,
            });
            patches.push(TerminalPatch::PutRow(Arc::clone(&row)));
            let clear_style = row
                .cells
                .last()
                .map_or_else(CellStyle::default, |cell| cell.style);
            patches.push(TerminalPatch::ClearToEndOfLine(clear_style));
        }
    }
    patches.push(TerminalPatch::MoveTo {
        column: desired.cursor.0.min(desired.width.saturating_sub(1)),
        row: desired.cursor.1.min(desired.height.saturating_sub(1)),
    });
    patches.push(TerminalPatch::ShowCursor(true));
    patches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_model_tracks_buffers_splits_and_tabs() {
        let mut model = ClientViewModel::new(DocumentId::new(10), "one");
        let second = model.add_buffer(DocumentId::new(11), "two");
        model.set_active_buffer(second).expect("select buffer");
        assert_eq!(model.active_buffer(), second);

        let first_window = model.active_window().id;
        let split = model.split_active(SplitAxis::Vertical).expect("split");
        assert_ne!(split, first_window);
        assert_eq!(model.windows.len(), 2);
        model
            .focus_window(WindowDirection::Left)
            .expect("focus left");
        assert_eq!(model.active_window().id, first_window);
        let lower = model
            .split_active(SplitAxis::Horizontal)
            .expect("lower split");
        model.focus_window(WindowDirection::Up).expect("focus up");
        assert_eq!(model.active_window().id, first_window);
        model
            .focus_window(WindowDirection::Down)
            .expect("focus down");
        assert_eq!(model.active_window().id, lower);
        model.close_active_window().expect("close lower split");
        model
            .focus_window(WindowDirection::Right)
            .expect("focus right");
        assert_eq!(model.active_window().id, split);
        model.close_active_window().expect("close split");
        assert_eq!(model.active_window().id, first_window);

        let first_tab = model.active_tab;
        model.new_tab(second).expect("new tab");
        assert_ne!(model.active_tab, first_tab);
        model.cycle_tab(-1);
        assert_eq!(model.active_tab, first_tab);
        model.cycle_tab(1);
        model.close_active_tab().expect("close tab");
        assert_eq!(model.active_tab, first_tab);
    }

    #[test]
    fn workspace_grid_renders_split_panes_and_active_cursor() {
        let mut model = ClientViewModel::new(DocumentId::new(10), "one");
        let second = model.add_buffer(DocumentId::new(11), "two");
        model.split_active(SplitAxis::Vertical).expect("split");
        model.set_active_buffer(second).expect("second pane");
        let first = model.buffers[0].id;
        let grid = ViewportLayout::new(15, 4).desired_workspace_grid(
            &model,
            &[
                (
                    first,
                    EngineFrame {
                        text: "left".into(),
                        cursor_byte: 0,
                    },
                ),
                (
                    second,
                    EngineFrame {
                        text: "right".into(),
                        cursor_byte: 5,
                    },
                ),
            ],
            "status",
            None,
        );
        let first_row: String = grid.rows[0]
            .cells
            .iter()
            .map(|cell| cell.grapheme.as_ref())
            .collect();
        assert!(first_row.contains("left"));
        assert!(first_row.contains('│'));
        assert!(first_row.contains("right"));
        assert!(grid.cursor.0 > 7);
    }

    #[test]
    fn workspace_hit_testing_matches_gutters_tabs_unicode_wraps_splits_and_status() {
        let mut model = ClientViewModel::new(DocumentId::new(10), "one");
        let first = model.buffers[0].id;
        let first_window = model.active_window().id;
        let second = model.add_buffer(DocumentId::new(11), "two");
        let second_window = model.split_active(SplitAxis::Vertical).expect("split");
        model.set_active_buffer(second).expect("second pane");
        let frames = [
            (
                first,
                EngineFrame {
                    text: "ab界cdef\n\txy\nlast".into(),
                    cursor_byte: 0,
                },
            ),
            (
                second,
                EngineFrame {
                    text: "right pane".into(),
                    cursor_byte: 0,
                },
            ),
        ];
        let mut layout = ViewportLayout::new(21, 7);
        layout.configure_dotfile_profile();

        let wide = layout
            .hit_test_workspace(&model, &frames, 6, 0, 1)
            .expect("wide grapheme");
        assert_eq!(
            (wide.window_id, wide.buffer_id, wide.byte),
            (first_window, first, 2)
        );
        let wrapped = layout
            .hit_test_workspace(&model, &frames, 3, 1, 1)
            .expect("wrapped continuation");
        assert_eq!(wrapped.byte, 8);
        let tab = layout
            .hit_test_workspace(&model, &frames, 4, 2, 1)
            .expect("tab expansion");
        assert_eq!(tab.byte, 10);
        let trailing = layout
            .hit_test_workspace(&model, &frames, 9, 2, 1)
            .expect("trailing cell");
        assert_eq!(trailing.byte, 12);
        let other_pane = layout
            .hit_test_workspace(&model, &frames, 14, 0, 1)
            .expect("second pane");
        assert_eq!(
            (other_pane.window_id, other_pane.buffer_id),
            (second_window, second)
        );
        assert!(
            layout
                .hit_test_workspace(&model, &frames, 10, 0, 1)
                .is_none()
        );
        assert!(
            layout
                .hit_test_workspace(&model, &frames, 3, 6, 1)
                .is_none()
        );
        let below_file = layout
            .hit_test_workspace(&model, &frames, 3, 5, 1)
            .expect("blank viewport row clamps to the last buffer line");
        assert_eq!(below_file.byte, "ab界cdef\n\txy\nlast".len() - 1);

        let tiny = ViewportLayout::new(1, 1);
        assert!(tiny.hit_test_workspace(&model, &frames, 0, 0, 1).is_none());

        model
            .focus_window_id(first_window)
            .expect("focus by click target");
        assert_eq!(model.active_window().id, first_window);
    }

    #[test]
    fn telescope_overlay_renders_results_selection_preview_and_prompt() {
        let frame = EngineFrame {
            text: "underlying editor".into(),
            cursor_byte: 0,
        };
        let mut layout = ViewportLayout::new(100, 30);
        let grid = layout.desired_editor_grid(&frame, "NORMAL", None);
        let grid = layout.apply_picker_overlay(
            grid,
            &PickerOverlay {
                title: "Find Files".into(),
                prompt: "main".into(),
                rows: vec![
                    PickerOverlayRow {
                        label: "README.md".into(),
                        detail: "".into(),
                    },
                    PickerOverlayRow {
                        label: "src/main.rs".into(),
                        detail: "Rust".into(),
                    },
                ],
                selected: 1,
                preview_title: "src/main.rs".into(),
                preview: "fn main() {\n    println!(\"wren\");\n}".into(),
                preview_scroll: 0,
                preview_highlight_line: Some(1),
                preview_decorations: vec![DecorationSpan {
                    range: 0..2,
                    priority: 100,
                    style: CellStyle {
                        bold: true,
                        foreground: Some(CellColor::Rgb(CatppuccinPalette::MOCHA.mauve)),
                        ..CellStyle::default()
                    },
                }],
                footer: "↑/↓ select  ⏎ open".into(),
            },
        );
        let rendered = grid
            .rows
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|cell| cell.grapheme.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Find Files (2)"));
        assert!(rendered.contains("❯ src/main.rs  Rust"));
        assert!(rendered.contains("println!(\"wren\")"));
        assert!(rendered.contains("❯ main"));
        assert_eq!(grid.cursor.1, 25);
    }

    #[test]
    fn telescope_preview_wraps_long_decorated_lines_inside_its_border() {
        let mut layout = ViewportLayout::new(100, 30);
        let grid = layout.desired_editor_grid(
            &EngineFrame {
                text: "underlying editor".into(),
                cursor_byte: 0,
            },
            "NORMAL",
            None,
        );
        let preview = "x".repeat(140);
        let grid = layout.apply_picker_overlay(
            grid,
            &PickerOverlay {
                title: "Find Files".into(),
                prompt: "".into(),
                rows: vec![PickerOverlayRow {
                    label: "src/main.rs".into(),
                    detail: "".into(),
                }],
                selected: 0,
                preview_title: "src/main.rs".into(),
                preview: preview.clone().into(),
                preview_scroll: 0,
                preview_highlight_line: Some(0),
                preview_decorations: vec![DecorationSpan {
                    range: 0..preview.len(),
                    priority: 100,
                    style: CellStyle {
                        bold: true,
                        foreground: Some(CellColor::Rgb(CatppuccinPalette::MOCHA.green)),
                        ..CellStyle::default()
                    },
                }],
                footer: "enter open".into(),
            },
        );

        // At this terminal size the floating picker's right border is column
        // 97 and its preview begins at 41. All 140 cells wrap inside it.
        for row in 5..25 {
            assert_eq!(grid.rows[row].cells[97].grapheme.as_ref(), "│");
        }
        let rendered_x = grid
            .rows
            .iter()
            .flat_map(|row| row.cells.iter())
            .filter(|cell| cell.grapheme.as_ref() == "x")
            .count();
        assert_eq!(rendered_x, preview.len());
        assert!(grid.rows.iter().all(|row| {
            row.cells
                .iter()
                .map(|cell| usize::from(cell.width))
                .sum::<usize>()
                <= 100
        }));
    }

    #[test]
    fn telescope_preview_uses_a_lighter_background_only_for_the_reference() {
        let mut layout = ViewportLayout::new(100, 30);
        let base = layout.desired_editor_grid(
            &EngineFrame {
                text: "underlying editor".into(),
                cursor_byte: 0,
            },
            "NORMAL",
            None,
        );
        let picker = PickerOverlay {
            title: "Jumplist".into(),
            prompt: "".into(),
            rows: vec![PickerOverlayRow {
                label: "src/main.rs:1:7".into(),
                detail: "language-server location".into(),
            }],
            selected: 0,
            preview_title: "src/main.rs".into(),
            preview: "alpha target omega".into(),
            preview_scroll: 0,
            preview_highlight_line: None,
            preview_decorations: vec![
                DecorationSpan {
                    range: 0..5,
                    priority: 100,
                    style: CellStyle {
                        foreground: Some(CellColor::Rgb(CatppuccinPalette::MOCHA.green)),
                        background: None,
                        ..CellStyle::default()
                    },
                },
                DecorationSpan {
                    range: 6..12,
                    priority: u32::MAX,
                    style: CellStyle {
                        foreground: None,
                        background: Some(CellColor::Rgb(CatppuccinPalette::MOCHA.surface0)),
                        ..CellStyle::default()
                    },
                },
            ],
            footer: "enter open".into(),
        };
        let geometry = layout.picker_geometry();
        let preview_column = geometry.column + 2 + geometry.result_width;

        let grid = layout.apply_picker_overlay(base, &picker);
        let row = &grid.rows[geometry.content_start];

        assert_eq!(
            row.cells[preview_column].style.foreground,
            Some(CellColor::Rgb(CatppuccinPalette::MOCHA.green))
        );
        assert_eq!(
            row.cells[preview_column].style.background,
            Some(CellColor::Rgb(CatppuccinPalette::MOCHA.mantle))
        );
        for cell in &row.cells[preview_column + 6..preview_column + 12] {
            assert_eq!(
                cell.style.background,
                Some(CellColor::Rgb(CatppuccinPalette::MOCHA.surface0))
            );
        }
        assert_eq!(
            row.cells[preview_column + 12].style.background,
            Some(CellColor::Rgb(CatppuccinPalette::MOCHA.mantle))
        );
    }

    #[test]
    fn dap_ui_overlay_renders_default_panels_without_replacing_the_editor() {
        let mut layout = ViewportLayout::new(100, 30);
        let grid = layout.desired_editor_grid(
            &EngineFrame {
                text: "fn main() {}".into(),
                cursor_byte: 0,
            },
            "NORMAL",
            None,
        );
        let grid = layout.apply_debug_overlay(
            grid,
            &DebugOverlay {
                scopes: "▸ Locals".into(),
                breakpoints: "● main.rs:1".into(),
                stacks: "▾ current thread".into(),
                watches: "watch value".into(),
                repl: "(lldb)".into(),
                console: "stopped".into(),
            },
        );
        let rendered = grid
            .rows
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|cell| cell.grapheme.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        for title in [
            "Scopes",
            "Breakpoints",
            "Stacks",
            "Watches",
            "REPL",
            "Console",
        ] {
            assert!(rendered.contains(title), "missing {title}: {rendered}");
        }
        assert!(rendered.contains("fn main()"));
    }

    #[test]
    fn catppuccin_flavors_are_exact_truecolor_and_overridable() {
        assert_eq!(
            CatppuccinPalette::MOCHA.base,
            RgbColor::new(0x1e, 0x1e, 0x2e)
        );
        assert_eq!(
            CatppuccinPalette::MOCHA.text,
            RgbColor::new(0xcd, 0xd6, 0xf4)
        );
        assert_eq!(
            CatppuccinPalette::LATTE.base,
            RgbColor::new(0xef, 0xf1, 0xf5)
        );
        let mut palette = CatppuccinPalette::for_flavor(CatppuccinFlavor::Macchiato);
        assert!(palette.set("mauve", RgbColor::new(1, 2, 3)));
        assert_eq!(palette.mauve, RgbColor::new(1, 2, 3));
        assert!(!palette.set("not-a-slot", RgbColor::new(1, 2, 3)));

        let mut layout = ViewportLayout::new(20, 2);
        layout.set_theme(CatppuccinPalette::LATTE);
        let grid = layout.desired_editor_grid(
            &EngineFrame {
                text: "x".into(),
                cursor_byte: 0,
            },
            "",
            None,
        );
        assert_eq!(
            grid.rows[0].cells[0].style,
            CellStyle {
                foreground: Some(CellColor::Rgb(CatppuccinPalette::LATTE.text)),
                background: Some(CellColor::Rgb(CatppuccinPalette::LATTE.base)),
                ..CellStyle::default()
            }
        );
    }

    #[test]
    fn every_terminal_row_erases_with_the_active_theme_background() {
        let mut layout = ViewportLayout::new(120, 40);
        layout.set_theme(CatppuccinPalette::MACCHIATO);
        let grid = layout.desired_editor_grid(
            &EngineFrame {
                text: "one short line".into(),
                cursor_byte: 0,
            },
            "NORMAL",
            None,
        );
        assert_eq!(grid.rows.len(), 40);
        assert!(grid.rows.iter().all(|row| !row.cells.is_empty()));
        let patches = diff(None, &grid);
        let clears = patches
            .iter()
            .filter_map(|patch| match patch {
                TerminalPatch::ClearToEndOfLine(style) => Some(style),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(clears.len(), 40);
        assert!(clears.iter().all(|style| {
            style.background == Some(CellColor::Rgb(CatppuccinPalette::MACCHIATO.base))
        }));
    }

    #[test]
    fn wraps_graphemes_without_splitting_them() {
        let frame = EngineFrame {
            text: "ab界c".into(),
            cursor_byte: "ab界".len(),
        };
        let grid = ViewportLayout::new(3, 3).desired_grid(&frame);
        assert_eq!(grid.rows[1].cells[0].grapheme.as_ref(), "界");
        assert_eq!(grid.cursor, (2, 1));
    }

    #[test]
    fn dotfile_profile_renders_relative_numbers_two_column_tabs_and_scrolloff() {
        let frame = EngineFrame {
            text: "one\n\ttwo\nthree\nfour\nfive\nsix\nseven\n".into(),
            cursor_byte: "one\n\tt".len(),
        };
        let mut layout = ViewportLayout::new(20, 5);
        layout.configure_dotfile_profile();
        layout.ensure_cursor_visible(&frame, 0);
        let grid = layout.desired_grid(&frame);
        let rows = grid
            .rows
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|cell| cell.grapheme.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(rows[0].starts_with(" 1 one"));
        assert!(rows[1].starts_with(" 2   two"));
        assert_eq!(grid.cursor.0, 6);

        let mut wide = ViewportLayout::new(90, 2);
        wide.configure_dotfile_profile();
        let wide_grid = wide.desired_grid(&EngineFrame {
            text: "short\n".into(),
            cursor_byte: 0,
        });
        let color_column = &wide_grid.rows[0].cells[82];
        assert_eq!(color_column.grapheme.as_ref(), " ");
        assert_eq!(
            color_column.style.background,
            Some(CellColor::Rgb(CatppuccinPalette::MOCHA.mantle))
        );
        assert!(!color_column.style.reverse);
    }

    #[test]
    fn text_past_the_color_limit_keeps_the_same_edge_on_wrapped_rows() {
        let mut layout = ViewportLayout::new(90, 3);
        layout.configure_dotfile_profile();
        let grid = layout.desired_grid(&EngineFrame {
            text: "x".repeat(180).into(),
            cursor_byte: 0,
        });

        for row in grid.rows.iter().take(2) {
            assert_eq!(row.cells[81].grapheme.as_ref(), "x");
            assert_eq!(
                row.cells[81].style.background,
                Some(CellColor::Rgb(CatppuccinPalette::MOCHA.base))
            );
            assert_eq!(row.cells[82].grapheme.as_ref(), "x");
            assert_eq!(
                row.cells[82].style.background,
                Some(CellColor::Rgb(CatppuccinPalette::MOCHA.mantle))
            );
            assert_eq!(row.cells[83].grapheme.as_ref(), "x");
            assert_eq!(
                row.cells[83].style.background,
                Some(CellColor::Rgb(CatppuccinPalette::MOCHA.mantle))
            );
        }
    }

    #[test]
    fn text_popup_crossing_color_limit_preserves_the_underlying_tail_background() {
        let mut layout = ViewportLayout::new(120, 8);
        layout.configure_dotfile_profile();
        let source = (0..8)
            .map(|_| format!("left{}right\n", " ".repeat(90)))
            .collect::<String>();
        let base = layout.desired_grid(&EngineFrame {
            text: source.into(),
            cursor_byte: 4,
        });
        let popup = TextPopup {
            title: "hover".into(),
            text: "a".repeat(80).into(),
            scroll: 0,
            cursor: None,
            decorations: Vec::new(),
        };

        let grid = layout.apply_text_popup(base, &popup);
        let interior = &grid.rows[2];

        assert_eq!(
            interior.cells[82].style.background,
            Some(CellColor::Rgb(CatppuccinPalette::MOCHA.mantle)),
            "popup must repaint the color-column cell with its own surface"
        );
        assert_eq!(
            interior.cells[100].style.background,
            Some(CellColor::Rgb(CatppuccinPalette::MOCHA.mantle)),
            "the terminal tail beyond the popup must retain the editor overflow background"
        );
    }

    #[test]
    fn text_popup_uses_empty_right_margin_instead_of_following_the_cursor() {
        let source = "fn one() {}\nfn two() {}\nfn three() {}\nfn four() {}\n";
        let popup = TextPopup {
            title: "hover".into(),
            text: "documentation".into(),
            scroll: 0,
            cursor: None,
            decorations: Vec::new(),
        };
        let mut layout = ViewportLayout::new(100, 12);

        let first = layout.desired_grid(&EngineFrame {
            text: source.into(),
            cursor_byte: 0,
        });
        let first_grid = layout.apply_text_popup(first.clone(), &popup);
        let first_placement = text_popup_placement(&first, 15, 3);
        let second = layout.desired_grid(&EngineFrame {
            text: source.into(),
            cursor_byte: source.find("four").expect("four"),
        });
        let second_grid = layout.apply_text_popup(second.clone(), &popup);
        let second_placement = text_popup_placement(&second, 15, 3);

        assert_eq!(first_placement, PopupPlacement { column: 84, row: 0 });
        assert_eq!(second_placement, first_placement);
        assert_eq!(first_grid.rows[0].cells[84].grapheme.as_ref(), "╭");
        assert_eq!(second_grid.rows[0].cells[84].grapheme.as_ref(), "╭");
        assert_eq!(first_grid.rows[0].cells[98].grapheme.as_ref(), "╮");
        assert_ne!(first_grid.rows[0].cells[99].grapheme.as_ref(), "╮");
    }

    #[test]
    fn text_popup_keeps_a_margin_after_its_right_border() {
        let mut layout = ViewportLayout::new(40, 8);
        let base = layout.desired_grid(&EngineFrame {
            text: "short\n".into(),
            cursor_byte: 0,
        });
        let popup = TextPopup {
            title: "hover".into(),
            text: "x".repeat(80).into(),
            scroll: 0,
            cursor: None,
            decorations: Vec::new(),
        };
        let placement = text_popup_placement(&base, 38, 5);

        let grid = layout.apply_text_popup(base, &popup);

        assert_eq!(placement.column, 1);
        assert_eq!(grid.rows[placement.row].cells[1].grapheme.as_ref(), "╭");
        assert_eq!(grid.rows[placement.row].cells[38].grapheme.as_ref(), "╮");
        assert_ne!(grid.rows[placement.row].cells[39].grapheme.as_ref(), "╮");
    }

    #[test]
    fn focused_text_popup_places_the_terminal_cursor_inside_its_content() {
        let mut layout = ViewportLayout::new(40, 8);
        let base = layout.desired_grid(&EngineFrame {
            text: "buffer\n".into(),
            cursor_byte: 0,
        });
        let popup = TextPopup {
            title: "hover".into(),
            text: "one\ntwo\nthree".into(),
            scroll: 0,
            cursor: Some((1, 1)),
            decorations: Vec::new(),
        };
        let placement = text_popup_placement(&base, 7, 5);

        let grid = layout.apply_text_popup(base, &popup);

        assert_eq!(grid.cursor, (placement.column + 2, placement.row + 2));
    }

    #[test]
    fn text_popup_finds_empty_right_edge_below_longer_lines() {
        let mut layout = ViewportLayout::new(40, 8);
        let grid = layout.desired_grid(&EngineFrame {
            text: "                              right\n                              edge\n"
                .into(),
            cursor_byte: 30,
        });

        assert_eq!(
            text_popup_placement(&grid, 12, 4),
            PopupPlacement { column: 27, row: 2 }
        );
    }

    #[test]
    fn one_cell_terminal_renders_status_without_indexing_past_the_grid() {
        let mut layout = ViewportLayout::new(1, 1);
        layout.configure_dotfile_profile();
        let model = ClientViewModel::new(DocumentId::new(1), "tiny");
        let frame = EngineFrame {
            text: "x".into(),
            cursor_byte: 0,
        };
        let grid =
            layout.desired_workspace_grid(&model, &[(BufferId::new(1), frame)], "NORMAL", None);
        assert_eq!(grid.rows.len(), 1);
    }

    #[test]
    fn controls_are_escaped_before_terminal_patches() {
        let frame = EngineFrame {
            text: "a\u{1b}\u{0}b".into(),
            cursor_byte: 0,
        };
        let grid = ViewportLayout::new(20, 2).desired_grid(&frame);
        let visible: String = grid.rows[0]
            .cells
            .iter()
            .map(|cell| cell.grapheme.as_ref())
            .collect();
        assert_eq!(visible, "a^[^@b");
        assert!(!visible.contains('\u{1b}'));
    }

    #[test]
    fn decorations_follow_byte_ranges_through_unicode_tabs_and_splits() {
        let mut model = ClientViewModel::new(DocumentId::new(10), "one");
        let second = model.add_buffer(DocumentId::new(11), "two");
        model.split_active(SplitAxis::Vertical).expect("split");
        model.set_active_buffer(second).expect("second pane");
        let first = model.buffers[0].id;
        let keyword = CellStyle {
            bold: true,
            foreground: Some(CellColor::Palette(12)),
            ..CellStyle::default()
        };
        let grid = ViewportLayout::new(20, 3).desired_workspace_grid_with_decorations(
            &model,
            &[
                (
                    first,
                    EngineFrame {
                        text: "界\tfn".into(),
                        cursor_byte: 0,
                    },
                ),
                (
                    second,
                    EngineFrame {
                        text: "let value".into(),
                        cursor_byte: 0,
                    },
                ),
            ],
            &[
                (
                    first,
                    vec![DecorationSpan {
                        range: "界\t".len().."界\tfn".len(),
                        priority: 100,
                        style: keyword,
                    }],
                ),
                (
                    second,
                    vec![DecorationSpan {
                        range: 0..3,
                        priority: 100,
                        style: keyword,
                    }],
                ),
            ],
            "",
            None,
        );
        let styled: String = grid
            .rows
            .iter()
            .flat_map(|row| row.cells.iter())
            .filter(|cell| cell.style == keyword)
            .map(|cell| cell.grapheme.as_ref())
            .collect();
        assert_eq!(styled, "fnlet");
    }

    #[test]
    fn signcolumn_number_decorations_do_not_overwrite_syntax_tokens() {
        let syntax = CellStyle {
            bold: true,
            foreground: Some(CellColor::Rgb(CatppuccinPalette::MOCHA.mauve)),
            ..CellStyle::default()
        };
        let git_sign = CellStyle {
            bold: true,
            foreground: Some(CellColor::Rgb(CatppuccinPalette::MOCHA.green)),
            ..CellStyle::default()
        };
        let mut layout = ViewportLayout::new(20, 2);
        layout.configure_dotfile_profile();
        let grid = layout.desired_editor_grid_with_line_decorations(
            &EngineFrame {
                text: "fn main() {}\n".into(),
                cursor_byte: 0,
            },
            "",
            None,
            &[DecorationSpan {
                range: 0..2,
                priority: 100,
                style: syntax,
            }],
            &[LineDecoration {
                line: 0,
                style: git_sign,
            }],
        );

        assert!(
            grid.rows[0].cells[..3]
                .iter()
                .all(|cell| cell.style.foreground == git_sign.foreground)
        );
        assert_eq!(grid.rows[0].cells[3].grapheme.as_ref(), "f");
        assert_eq!(grid.rows[0].cells[3].style.foreground, syntax.foreground);
        assert_eq!(grid.rows[0].cells[4].grapheme.as_ref(), "n");
        assert_eq!(grid.rows[0].cells[4].style.foreground, syntax.foreground);
    }

    #[test]
    fn cursor_scrolls_into_view_and_prompt_owns_last_row() {
        let frame = EngineFrame {
            text: "0\n1\n2\n3\n4\n".into(),
            cursor_byte: "0\n1\n2\n3\n".len(),
        };
        let mut layout = ViewportLayout::new(10, 3);
        layout.ensure_cursor_visible(&frame, 1);
        assert_eq!(layout.top_line, 3);
        let grid = layout.desired_editor_grid(&frame, "", Some(":w"));
        assert_eq!(grid.cursor, (2, 2));
    }

    #[test]
    fn shrinking_rows_emits_clear_to_end() {
        let old = ViewportLayout::new(20, 2).desired_grid(&EngineFrame {
            text: "long line".into(),
            cursor_byte: 0,
        });
        let new = ViewportLayout::new(20, 2).desired_grid(&EngineFrame {
            text: "x".into(),
            cursor_byte: 0,
        });
        assert!(
            diff(Some(&old), &new)
                .iter()
                .any(|patch| matches!(patch, TerminalPatch::ClearToEndOfLine(_)))
        );
    }

    #[test]
    fn unchanged_rows_are_arc_reused_while_frame_epochs_advance() {
        let frame = EngineFrame {
            text: "same\nrows".into(),
            cursor_byte: 0,
        };
        let mut layout = ViewportLayout::new(20, 2);
        let first = layout.desired_grid(&frame);
        let second = layout.desired_grid(&EngineFrame {
            cursor_byte: 5,
            ..frame
        });
        assert!(second.epoch > first.epoch);
        assert!(Arc::ptr_eq(&first.rows[0], &second.rows[0]));
        assert!(Arc::ptr_eq(&first.rows[1], &second.rows[1]));
        assert_eq!(second.cursor, (0, 1));
    }

    #[test]
    fn unicode_line_edits_only_rebuild_the_changed_visual_rows() {
        let mut layout = ViewportLayout::new(30, 3);
        let first = layout.desired_grid(&EngineFrame {
            text: "界 alpha\nβeta\ngamma".into(),
            cursor_byte: 0,
        });
        let second = layout.desired_grid(&EngineFrame {
            text: "界λ alpha\nβeta\ngamma".into(),
            cursor_byte: "界λ".len(),
        });

        assert!(!Arc::ptr_eq(&first.rows[0], &second.rows[0]));
        assert!(Arc::ptr_eq(&first.rows[1], &second.rows[1]));
        assert!(Arc::ptr_eq(&first.rows[2], &second.rows[2]));
        assert_eq!(second.cursor, (3, 0));
        assert_eq!(
            second.rows[0]
                .cells
                .iter()
                .map(|cell| cell.grapheme.as_ref())
                .collect::<String>(),
            "界λ alpha"
        );
    }

    #[test]
    fn one_line_viewport_scroll_reuses_visible_unicode_rows() {
        let frame = EngineFrame {
            text: "αlpha\nβeta\nγamma\nδelta".into(),
            cursor_byte: 0,
        };
        let mut layout = ViewportLayout::new(30, 3);
        let first = layout.desired_grid(&frame);
        layout.top_line = 1;
        let second = layout.desired_grid(&frame);

        assert!(Arc::ptr_eq(&first.rows[1], &second.rows[0]));
        assert!(Arc::ptr_eq(&first.rows[2], &second.rows[1]));
        assert_eq!(
            second.rows[2]
                .cells
                .iter()
                .map(|cell| cell.grapheme.as_ref())
                .collect::<String>(),
            "δelta"
        );
    }

    #[test]
    fn half_page_viewport_scroll_reuses_the_overlapping_rows_in_both_directions() {
        let text = (0..20)
            .map(|line| format!("line {line:02}\n"))
            .collect::<String>();
        let frame = EngineFrame {
            text: text.into(),
            cursor_byte: 0,
        };
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
        assert_eq!(
            up.rows[0]
                .cells
                .iter()
                .map(|cell| cell.grapheme.as_ref())
                .collect::<String>(),
            "line 02"
        );
        assert_eq!(
            up.rows[1]
                .cells
                .iter()
                .map(|cell| cell.grapheme.as_ref())
                .collect::<String>(),
            "line 03"
        );
    }

    #[test]
    fn distant_viewport_jump_reuses_a_recent_view_without_rerendering_rows() {
        let text = (0..100)
            .map(|line| format!("line {line:02}\n"))
            .collect::<String>();
        let frame = EngineFrame {
            text: text.into(),
            cursor_byte: 0,
        };
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
        let text = (0..20)
            .map(|line| format!("line {line:02}\n"))
            .collect::<String>();
        let text = FrameText::from(text);
        let mut layout = ViewportLayout::new(30, 8);
        layout.configure_dotfile_profile();
        let row_text = |row: &CellRow| {
            row.cells
                .iter()
                .map(|cell| cell.grapheme.as_ref())
                .collect::<String>()
        };

        let first = layout.desired_editor_grid(
            &EngineFrame {
                text: text.clone(),
                cursor_byte: 0,
            },
            "",
            None,
        );
        layout.top_line = 4;
        let down = layout.desired_editor_grid(
            &EngineFrame {
                text: text.clone(),
                cursor_byte: text.byte_of_line(4),
            },
            "",
            None,
        );
        assert!(layout.cached_logical_rows.keys().all(|line| *line < 4));
        assert!(row_text(&down.rows[0]).starts_with(" 5 line 04"));

        layout.top_line = 0;
        let up = layout.desired_editor_grid(
            &EngineFrame {
                text,
                cursor_byte: 0,
            },
            "",
            None,
        );
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
            left: vec![StatusSegment {
                text: " LEFT ".into(),
                style: CellStyle {
                    bold: true,
                    ..CellStyle::default()
                },
            }],
            right: vec![StatusSegment {
                text: " R ".into(),
                style: CellStyle {
                    reverse: true,
                    ..CellStyle::default()
                },
            }],
        };
        let base = || DesiredGrid {
            epoch: 0,
            width: 20,
            height: 3,
            rows: (0..3).map(|_| Arc::new(CellRow::default())).collect(),
            cursor: (0, 0),
        };
        let first = layout.apply_status_overlay(base(), &status);
        let second = layout.apply_status_overlay(base(), &status);
        assert!(Arc::ptr_eq(&first.rows[2], &second.rows[2]));
        let rendered = first.rows[2]
            .cells
            .iter()
            .map(|cell| cell.grapheme.as_ref())
            .collect::<String>();
        assert_eq!(rendered, " LEFT             R ");
    }

    #[test]
    fn production_workspace_retains_unchanged_pane_rows() {
        let model = ClientViewModel::new(DocumentId::new(10), "main.rs");
        let buffer_id = model.active_buffer();
        let window_id = model.active_window().id;
        let mut layout = ViewportLayout::new(30, 4);
        let first = layout.desired_workspace_grid(
            &model,
            &[(
                buffer_id,
                EngineFrame {
                    text: "界 alpha\nβeta\ngamma".into(),
                    cursor_byte: 0,
                },
            )],
            "NORMAL",
            None,
        );
        let cached_second_row = Arc::clone(&layout.pane_layouts[&window_id].cached_rows[1]);
        let second = layout.desired_workspace_grid(
            &model,
            &[(
                buffer_id,
                EngineFrame {
                    text: "界λ alpha\nβeta\ngamma".into(),
                    cursor_byte: "界λ".len(),
                },
            )],
            "NORMAL",
            None,
        );

        assert!(Arc::ptr_eq(
            &cached_second_row,
            &layout.pane_layouts[&window_id].cached_rows[1]
        ));
        assert!(Arc::ptr_eq(&first.rows[1], &second.rows[1]));
        assert_eq!(second.cursor, (3, 0));
    }

    #[test]
    fn offscreen_line_edits_do_not_replace_visible_rows() {
        let mut layout = ViewportLayout::new(30, 2);
        layout.top_line = 1;
        let first = layout.desired_grid(&EngineFrame {
            text: "alpha\nbeta\ngamma".into(),
            cursor_byte: "alpha\n".len(),
        });
        let second = layout.desired_grid(&EngineFrame {
            text: "alphax\nbeta\ngamma".into(),
            cursor_byte: "alphax\n".len(),
        });

        assert!(Arc::ptr_eq(&first.rows[0], &second.rows[0]));
        assert!(Arc::ptr_eq(&first.rows[1], &second.rows[1]));
        assert_eq!(
            second.rows[0]
                .cells
                .iter()
                .map(|cell| cell.grapheme.as_ref())
                .collect::<String>(),
            "beta"
        );
    }

    #[test]
    fn ordered_decoration_sweep_matches_full_overlap_resolution() {
        let decorations = vec![
            DecorationSpan {
                range: 4..9,
                priority: 20,
                style: CellStyle {
                    foreground: Some(CellColor::Palette(2)),
                    ..CellStyle::default()
                },
            },
            DecorationSpan {
                range: 0..12,
                priority: 10,
                style: CellStyle {
                    bold: true,
                    foreground: Some(CellColor::Palette(1)),
                    ..CellStyle::default()
                },
            },
            DecorationSpan {
                range: 7..11,
                priority: 20,
                style: CellStyle {
                    underline: true,
                    foreground: Some(CellColor::Palette(3)),
                    ..CellStyle::default()
                },
            },
            DecorationSpan {
                range: 2..5,
                priority: 30,
                style: CellStyle {
                    background: Some(CellColor::Palette(4)),
                    ..CellStyle::default()
                },
            },
        ];
        let mut resolver = DecorationResolver::new(&decorations);
        for start in 0..14 {
            let range = start..start + 1;
            assert_eq!(
                resolver.style(range.clone()),
                decoration_style(&decorations, range)
            );
        }
    }
}
