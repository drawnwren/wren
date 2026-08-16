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
use wren_types::{BufferId, DocumentId, FloatingSurfaceId, TabId, ViewId, WindowId};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPopup {
    pub title: Box<str>,
    pub text: Box<str>,
    pub scroll: usize,
    pub decorations: Vec<DecorationSpan>,
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

const INLINE_GRAPHEME_BYTES: usize = 15;

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
    decorations: Vec<DecorationSpan>,
    line_decorations: Vec<LineDecoration>,
    relative_cursor_line: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalPatch {
    Clear,
    ClearToEndOfLine(CellStyle),
    MoveTo { column: usize, row: usize },
    SetStyle(CellStyle),
    Put(Cell),
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
    cached_editor_render: Option<CachedEditorRender>,
    pane_layouts: BTreeMap<WindowId, Box<ViewportLayout>>,
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
            cached_editor_render: None,
            pane_layouts: BTreeMap::new(),
        }
    }

    pub fn set_theme(&mut self, theme: CatppuccinPalette) {
        if self.theme != theme {
            self.theme = theme;
            self.cached_rows.clear();
            self.cached_editor_render = None;
            self.pane_layouts.clear();
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
        self.cached_rows.clear();
        self.cached_editor_render = None;
        self.pane_layouts.clear();
        self.epoch = self.epoch.saturating_add(1);
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.epoch = self.epoch.saturating_add(1);
        self.cached_rows.clear();
        self.cached_editor_render = None;
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
        let cursor_line = self.frame_line_of_byte(frame, cursor);
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

    fn frame_line_of_byte(&mut self, frame: &EngineFrame, byte: usize) -> usize {
        frame.text.line_of_byte(byte)
    }

    fn frame_byte_of_line(&mut self, frame: &EngineFrame, line: usize) -> usize {
        frame.text.byte_of_line(line)
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
        if self.editor_render_is_cached(frame, status, prompt, decorations, line_decorations) {
            return self.reuse_editor_render(frame, status, prompt);
        }
        if let Some(grid) =
            self.update_cached_editor_viewport(frame, status, prompt, decorations, line_decorations)
        {
            return grid;
        }
        if let Some(grid) =
            self.update_cached_editor_line(frame, status, prompt, decorations, line_decorations)
        {
            return grid;
        }
        let reserved_rows = usize::from(!status.is_empty() || prompt.is_some());
        let content_height = self.height.saturating_sub(reserved_rows).max(1);
        let start_byte = self.frame_byte_of_line(frame, self.top_line);
        let gutter = if self.line_numbers {
            self.number_width.min(self.width.saturating_sub(1))
        } else {
            0
        };
        let mut builder = GridBuilder::new(
            self.width.saturating_sub(gutter),
            content_height,
            self.tab_width,
            start_byte,
            self.top_line,
        );
        let end_byte = frame
            .text
            .byte_of_line(self.top_line.saturating_add(content_height));
        let visible = frame.text.slice(start_byte..end_byte);
        builder.push_grapheme_document(&visible, frame.cursor_byte, decorations);
        let mut content_row_lines = builder.row_lines.clone();
        content_row_lines.resize(content_height, None);
        content_row_lines.truncate(content_height);
        let mut rows = builder.rows;
        let mut cursor = builder.cursor.unwrap_or((0, 0));
        if gutter > 0 {
            let cursor_line = self.frame_line_of_byte(frame, frame.cursor_byte);
            prepend_line_numbers(
                &mut rows,
                &builder.row_lines,
                cursor_line,
                gutter,
                self.relative_numbers,
                line_decorations,
            );
            cursor.0 = cursor.0.saturating_add(gutter);
        }
        if let Some(column) = self.color_column {
            mark_color_column(
                &mut rows,
                gutter.saturating_add(column.saturating_sub(1)),
                self.width,
                self.theme.mantle,
            );
        }

        if reserved_rows == 1 {
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
            rows.resize_with(content_height, CellRow::default);
            rows.push(row_from_text(label, self.width, style, self.tab_width));
            if prompt.is_some() {
                let input = label.split("  │  ").next().unwrap_or(label);
                let column = display_width(input, self.tab_width).min(self.width.saturating_sub(1));
                cursor = (column, self.height.saturating_sub(1));
            }
        }

        rows.resize_with(self.height, CellRow::default);
        rows.truncate(self.height);
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
        self.cached_editor_render = Some(CachedEditorRender {
            text: frame.text.clone(),
            content_row_lines,
            top_line: self.top_line,
            width: self.width,
            height: self.height,
            tab_width: self.tab_width,
            line_numbers: self.line_numbers,
            relative_numbers: self.relative_numbers,
            number_width: self.number_width,
            color_column: self.color_column,
            theme: self.theme,
            status: status.into(),
            prompt: prompt.map(Into::into),
            decorations: decorations.to_vec(),
            line_decorations: line_decorations.to_vec(),
            relative_cursor_line: self
                .relative_numbers
                .then(|| self.frame_line_of_byte(frame, frame.cursor_byte)),
        });
        self.epoch = self.epoch.saturating_add(1);
        DesiredGrid {
            epoch: self.epoch,
            width: self.width,
            height: self.height,
            rows,
            cursor,
        }
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
        self.cached_rows.len() == self.height
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
            && cached.decorations == decorations
            && cached.line_decorations == line_decorations
            && cached.relative_cursor_line
                == self
                    .relative_numbers
                    .then(|| frame.text.line_of_byte(frame.cursor_byte))
    }

    fn update_cached_editor_viewport(
        &mut self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) -> Option<DesiredGrid> {
        let cached = self.cached_editor_render.as_ref()?;
        if !cached.text.same_snapshot(&frame.text)
            || self.top_line != cached.top_line.saturating_add(1)
            || !self.editor_render_stable_context_is_cached(
                frame,
                status,
                prompt,
                decorations,
                line_decorations,
            )
        {
            return None;
        }
        let reserved_rows = usize::from(!status.is_empty() || prompt.is_some());
        let content_height = self.height.saturating_sub(reserved_rows).max(1);
        if cached.content_row_lines.len() != content_height
            || !cached
                .content_row_lines
                .iter()
                .enumerate()
                .all(|(offset, line)| *line == Some(cached.top_line.saturating_add(offset)))
        {
            return None;
        }

        self.cached_rows[..content_height].rotate_left(1);
        let logical_line = self
            .top_line
            .saturating_add(content_height.saturating_sub(1));
        let start = self.frame_byte_of_line(frame, logical_line);
        let end = frame
            .text
            .byte_of_line(logical_line.saturating_add(1))
            .min(frame.text.len());
        let end = if end > start && frame.text.slice(end - 1..end).as_ref() == "\n" {
            end - 1
        } else {
            end
        };
        let cursor_line = self.cached_frame_line_of_byte(frame, frame.cursor_byte);
        let mut rendered = self.render_editor_line(
            &frame.text,
            logical_line,
            cursor_line,
            start,
            end,
            1,
            decorations,
            line_decorations,
        );
        self.cached_rows[content_height - 1] = Arc::new(rendered.pop()?);

        let cached = self.cached_editor_render.as_mut()?;
        cached.top_line = self.top_line;
        cached.content_row_lines.rotate_left(1);
        cached.content_row_lines[content_height - 1] = Some(logical_line);
        Some(self.reuse_editor_render(frame, status, prompt))
    }

    fn update_cached_editor_line(
        &mut self,
        frame: &EngineFrame,
        status: &str,
        prompt: Option<&str>,
        decorations: &[DecorationSpan],
        line_decorations: &[LineDecoration],
    ) -> Option<DesiredGrid> {
        let cached = self.cached_editor_render.as_ref()?;
        if self.cached_rows.len() != self.height
            || cached.top_line != self.top_line
            || cached.width != self.width
            || cached.height != self.height
            || cached.tab_width != self.tab_width
            || cached.line_numbers != self.line_numbers
            || cached.relative_numbers != self.relative_numbers
            || cached.number_width != self.number_width
            || cached.color_column != self.color_column
            || cached.theme != self.theme
            || cached.prompt.as_deref() != prompt
            || cached.status.is_empty() != status.is_empty()
            || cached.line_decorations != line_decorations
            || cached.relative_cursor_line
                != self
                    .relative_numbers
                    .then(|| frame.text.line_of_byte(frame.cursor_byte))
        {
            return None;
        }
        let change = single_line_change(&cached.text, &frame.text)?;
        if !decorations_match_outside_change(&cached.decorations, decorations, change) {
            return None;
        }
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
        let screen_row = visual_rows_between_lines(
            &cached.text,
            self.top_line,
            change.line,
            content_width,
            self.tab_width,
        );
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
        let cursor_line = self.frame_line_of_byte(frame, frame.cursor_byte);
        if screen_row < content_height {
            let visible_rows = content_height.saturating_sub(screen_row).min(new_rows);
            let rendered = self.render_editor_line(
                &frame.text,
                change.line,
                cursor_line,
                change.new_start,
                change.new_end,
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
        }
        let cached = self.cached_editor_render.as_mut()?;
        cached.text.clone_from(&frame.text);
        cached.status = status.into();
        cached.decorations = decorations.to_vec();
        cached.relative_cursor_line = self.relative_numbers.then_some(cursor_line);
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
        Some(self.reuse_editor_render(frame, status, prompt))
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
        let start = self.frame_byte_of_line(frame, self.top_line);
        let cursor = frame.cursor_byte.min(frame.text.len());
        let visible = frame.text.slice(start..cursor.max(start));
        let mut cursor = cursor_visual_position(
            &visible,
            0,
            visible.len(),
            self.width.saturating_sub(gutter).max(1),
            content_height,
            self.tab_width,
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
        let reserved_rows = usize::from(!status.is_empty() || prompt.is_some());
        let content_height = self.height.saturating_sub(reserved_rows).max(1);
        let tab = model.active_tab();
        let mut panes = Vec::new();
        split_rectangles(
            &tab.root,
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

        if panes.len() == 1 {
            let (window_id, rectangle) = panes[0];
            if rectangle.column == 0
                && rectangle.row == 0
                && rectangle.width == self.width
                && rectangle.height == content_height
                && let Some(window) = model.windows.iter().find(|window| window.id == window_id)
                && let Some((_, frame)) = frames
                    .iter()
                    .find(|(buffer_id, _)| *buffer_id == window.buffer_id)
            {
                let buffer_decorations = decorations
                    .iter()
                    .find(|(buffer_id, _)| *buffer_id == window.buffer_id)
                    .map_or(&[][..], |(_, spans)| spans.as_slice());
                let buffer_line_decorations = line_decorations
                    .iter()
                    .find(|(buffer_id, _)| *buffer_id == window.buffer_id)
                    .map_or(&[][..], |(_, spans)| spans.as_slice());
                let grid = self.render_workspace_pane(
                    window_id,
                    rectangle,
                    window.top_line,
                    frame,
                    buffer_decorations,
                    buffer_line_decorations,
                );
                return self.finish_single_pane_workspace(grid, status, prompt);
            }
        }

        let mut rows = vec![CellRow::default(); self.height];
        let mut cursor = (0, 0);
        for (window_id, rectangle) in panes {
            let Some(window) = model.windows.iter().find(|window| window.id == window_id) else {
                continue;
            };
            let Some((_, frame)) = frames
                .iter()
                .find(|(buffer_id, _)| *buffer_id == window.buffer_id)
            else {
                continue;
            };
            let buffer_decorations = decorations
                .iter()
                .find(|(buffer_id, _)| *buffer_id == window.buffer_id)
                .map_or(&[][..], |(_, spans)| spans.as_slice());
            let buffer_line_decorations = line_decorations
                .iter()
                .find(|(buffer_id, _)| *buffer_id == window.buffer_id)
                .map_or(&[][..], |(_, spans)| spans.as_slice());
            let grid = self.render_workspace_pane(
                window_id,
                rectangle,
                window.top_line,
                frame,
                buffer_decorations,
                buffer_line_decorations,
            );
            for (pane_row, source) in grid.rows.iter().enumerate() {
                let target_row = rectangle.row + pane_row;
                let Some(target) = rows.get_mut(target_row) else {
                    continue;
                };
                pad_row_to(target, rectangle.column);
                target.cells.extend(source.cells.iter().cloned());
            }
            if window_id == tab.active_window {
                cursor = (
                    rectangle.column + grid.cursor.0,
                    rectangle.row + grid.cursor.1,
                );
            }
        }
        draw_split_borders(&tab.root, 0, 0, self.width, content_height, &mut rows);

        if reserved_rows == 1 {
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
            rows[status_row] = row_from_text(label, self.width, style, self.tab_width);
            if prompt.is_some() {
                let input = label.split("  │  ").next().unwrap_or(label);
                cursor = (
                    display_width(input, self.tab_width).min(self.width.saturating_sub(1)),
                    self.height.saturating_sub(1),
                );
            }
        }

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
        decorations: &[DecorationSpan],
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
        pane.desired_editor_grid_with_line_decorations(
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
        let base_style = CellStyle {
            foreground: Some(CellColor::Rgb(self.theme.text)),
            background: Some(CellColor::Rgb(self.theme.surface0)),
            ..CellStyle::default()
        };
        let border_style = CellStyle {
            foreground: Some(CellColor::Rgb(self.theme.blue)),
            background: Some(CellColor::Rgb(self.theme.base)),
            ..CellStyle::default()
        };
        let selected_style = CellStyle {
            bold: true,
            foreground: Some(CellColor::Rgb(self.theme.text)),
            background: Some(CellColor::Rgb(self.theme.surface2)),
            ..CellStyle::default()
        };
        draw_popup_frame(
            &mut rows,
            menu_column,
            menu_row,
            menu_width,
            menu_height,
            base_style,
            border_style,
            "completion",
        );
        for (offset, item) in completion
            .rows
            .iter()
            .skip(start)
            .take(visible_rows)
            .enumerate()
        {
            let item_index = start + offset;
            let marker = if completion.selected == Some(item_index) {
                "❯"
            } else {
                " "
            };
            let label = format!(
                "{marker} {}{} [{}]",
                item.label,
                if item.detail.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", item.detail)
                },
                item.source
            );
            paint_text(
                &mut rows[menu_row + 1 + offset],
                menu_column + 1,
                menu_width.saturating_sub(2),
                &label,
                if completion.selected == Some(item_index) {
                    selected_style
                } else {
                    base_style
                },
            );
        }

        self.draw_completion_documentation(
            &mut rows,
            completion,
            Rect {
                column: menu_column,
                row: menu_row,
                width: menu_width,
                height: menu_height,
            },
            base_style,
            border_style,
        );
        self.finish_overlay(grid, rows, cursor)
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

    /// Rounded cursor-relative float used by LSP hover/signature help,
    /// diagnostics, Git previews and other `open_floating_preview` surfaces.
    #[must_use]
    pub fn apply_text_popup(&mut self, grid: DesiredGrid, popup: &TextPopup) -> DesiredGrid {
        if popup.text.is_empty() || self.width < 8 || self.height < 4 {
            return grid;
        }
        let cursor = grid.cursor;
        let max_inner_width = self.width.saturating_sub(4).clamp(4, 80);
        let lines = wrap_popup_text(&popup.text, max_inner_width, self.tab_width);
        let inner_width = lines
            .iter()
            .map(|line| display_width(&line.text, self.tab_width))
            .max()
            .unwrap_or(1)
            .clamp(1, max_inner_width);
        let width = inner_width + 2;
        let visible_lines = lines
            .len()
            .saturating_sub(popup.scroll)
            .min(self.height.saturating_sub(3).max(1));
        let height = visible_lines + 2;
        let column = cursor.0.min(self.width.saturating_sub(width));
        let row = if cursor.1 + height + 1 < self.height {
            cursor.1 + 1
        } else {
            cursor.1.saturating_sub(height)
        };
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
        for (offset, line) in lines
            .iter()
            .skip(popup.scroll)
            .take(visible_lines)
            .enumerate()
        {
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
        let Some((_, rectangle)) = panes
            .into_iter()
            .find(|(window_id, _)| *window_id == tab.active_window)
        else {
            return grid;
        };
        let Some(window) = model
            .windows
            .iter()
            .find(|window| window.id == tab.active_window)
        else {
            return grid;
        };
        let gutter = if self.line_numbers {
            self.number_width.min(rectangle.width.saturating_sub(1))
        } else {
            0
        };
        let content_width = rectangle.width.saturating_sub(gutter).max(1);
        let start_byte = self.frame_byte_of_line(frame, window.top_line);
        let wanted = overlay
            .targets
            .iter()
            .filter(|target| target.byte >= start_byte)
            .map(|target| (target.byte, target.label.as_ref()))
            .collect::<BTreeMap<_, _>>();
        let mut positions = Vec::new();
        let mut byte = start_byte;
        let mut screen_row = 0_usize;
        let mut screen_column = 0_usize;
        let end_byte = frame
            .text
            .byte_of_line(window.top_line.saturating_add(rectangle.height.max(1)));
        let visible = frame.text.slice(start_byte..end_byte);
        for grapheme in visible.graphemes(true) {
            if let Some(label) = wanted.get(&byte)
                && screen_row < rectangle.height
            {
                positions.push((screen_row, screen_column, *label));
            }
            if grapheme == "\n" {
                screen_row = screen_row.saturating_add(1);
                screen_column = 0;
            } else if grapheme == "\t" {
                let spaces = self.tab_width - (screen_column % self.tab_width);
                for _ in 0..spaces {
                    if screen_column + 1 > content_width && screen_column > 0 {
                        screen_row = screen_row.saturating_add(1);
                        screen_column = 0;
                    }
                    screen_column = screen_column.saturating_add(1);
                }
            } else {
                for escaped in escape_grapheme(grapheme) {
                    let width = UnicodeWidthStr::width(escaped.as_str()).max(1);
                    if screen_column.saturating_add(width) > content_width && screen_column > 0 {
                        screen_row = screen_row.saturating_add(1);
                        screen_column = 0;
                    }
                    screen_column = screen_column.saturating_add(width);
                }
            }
            byte = byte.saturating_add(grapheme.len());
            if screen_row >= rectangle.height {
                break;
            }
        }
        let mut rows = overlay_rows(&grid);
        let style = CellStyle {
            bold: true,
            foreground: Some(CellColor::Rgb(self.theme.base)),
            background: Some(CellColor::Rgb(self.theme.peach)),
            ..CellStyle::default()
        };
        for (target_row, target_column, label) in positions {
            let row = rectangle.row.saturating_add(target_row);
            let column = rectangle
                .column
                .saturating_add(gutter)
                .saturating_add(target_column);
            let available = rectangle
                .column
                .saturating_add(rectangle.width)
                .saturating_sub(column);
            if let Some(target) = rows.get_mut(row) {
                paint_text(
                    target,
                    column,
                    display_width(label, self.tab_width).min(available),
                    label,
                    style,
                );
            }
        }
        let cursor = grid.cursor;
        self.finish_overlay(grid, rows, cursor)
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
        let mut row = row_from_text(
            &" ".repeat(self.width),
            self.width,
            CellStyle {
                foreground: Some(CellColor::Rgb(self.theme.subtext1)),
                background: Some(CellColor::Rgb(self.theme.mantle)),
                ..CellStyle::default()
            },
            1,
        );
        let mut left_column = 0;
        for segment in &status.left {
            let width = display_width(&segment.text, 1).min(self.width.saturating_sub(left_column));
            paint_text(&mut row, left_column, width, &segment.text, segment.style);
            left_column = left_column.saturating_add(width);
        }
        let right_width = status
            .right
            .iter()
            .map(|segment| display_width(&segment.text, 1))
            .sum::<usize>()
            .min(self.width.saturating_sub(left_column));
        let mut right_column = self.width.saturating_sub(right_width);
        for segment in &status.right {
            let width =
                display_width(&segment.text, 1).min(self.width.saturating_sub(right_column));
            paint_text(&mut row, right_column, width, &segment.text, segment.style);
            right_column = right_column.saturating_add(width);
        }
        apply_theme_to_rows(std::slice::from_mut(&mut row), self.theme);
        if let Some(target) = rows.get_mut(row_index) {
            *target = Arc::new(row);
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

#[derive(Debug, Clone, Copy)]
struct Rect {
    column: usize,
    row: usize,
    width: usize,
    height: usize,
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
        let source_line = source_with_newline
            .strip_suffix('\n')
            .unwrap_or(source_with_newline)
            .strip_suffix('\r')
            .unwrap_or_else(|| {
                source_with_newline
                    .strip_suffix('\n')
                    .unwrap_or(source_with_newline)
            });
        if source_line.is_empty() {
            output.push(WrappedPopupLine {
                text: String::new(),
                source_start: source_offset,
                source_line: source_line_number,
            });
            source_offset += source_with_newline.len();
            continue;
        }
        if display_width(source_line, tab_width) <= width {
            output.push(WrappedPopupLine {
                text: source_line.to_owned(),
                source_start: source_offset,
                source_line: source_line_number,
            });
            source_offset += source_with_newline.len();
            continue;
        }

        let mut start = 0;
        while start < source_line.len() {
            let mut used = 0_usize;
            let mut end = start;
            let mut whitespace = None;
            for (relative, grapheme) in source_line[start..].grapheme_indices(true) {
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
            if end == start {
                end = source_line[start..]
                    .graphemes(true)
                    .next()
                    .map_or(source_line.len(), |grapheme| start + grapheme.len());
            }
            let (display_end, mut next) = match whitespace {
                Some((space_start, space_end))
                    if space_start > start && end < source_line.len() =>
                {
                    (space_start, space_end)
                }
                _ => (end, end),
            };
            output.push(WrappedPopupLine {
                text: source_line[start..display_end].trim_end().to_owned(),
                source_start: source_offset + start,
                source_line: source_line_number,
            });
            while next < source_line.len() {
                let grapheme = source_line[next..]
                    .graphemes(true)
                    .next()
                    .unwrap_or_default();
                if !grapheme.chars().all(char::is_whitespace) {
                    break;
                }
                next += grapheme.len();
            }
            start = next;
        }
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
        let overlay = decoration_style(decorations, range);
        if overlay != CellStyle::default() {
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

    fn push_document(&mut self, text: &str, cursor_byte: usize, decorations: &[DecorationSpan]) {
        let visible = text.get(self.document_start..).unwrap_or_default();
        self.push_grapheme_document(visible, cursor_byte, decorations);
    }

    fn push_grapheme_document(
        &mut self,
        visible: &str,
        cursor_byte: usize,
        decorations: &[DecorationSpan],
    ) {
        let mut absolute_byte = self.document_start;
        let mut decoration_resolver = DecorationResolver::new(decorations);
        if cursor_byte < self.document_start {
            self.cursor = Some((0, 0));
        }
        for grapheme in visible.graphemes(true) {
            let grapheme_start = absolute_byte;
            if absolute_byte == cursor_byte {
                self.cursor = Some((self.column, self.row));
            }
            absolute_byte += grapheme.len();
            let style = if decorations.is_empty() {
                CellStyle::default()
            } else {
                decoration_resolver.style(grapheme_start..absolute_byte)
            };
            if grapheme == "\n" {
                self.logical_line = self.logical_line.saturating_add(1);
                if !self.next_row(Some(self.logical_line)) {
                    break;
                }
                continue;
            }
            if grapheme == "\t" {
                let spaces = self.tab_width - (self.column % self.tab_width);
                for _ in 0..spaces {
                    if !self.push_cell(" ", style) {
                        return;
                    }
                }
                continue;
            }
            if grapheme.chars().all(|character| !character.is_control()) {
                if !self.push_cell(grapheme, style) {
                    return;
                }
            } else {
                for escaped in escape_grapheme(grapheme) {
                    if !self.push_cell(&escaped, style) {
                        return;
                    }
                }
            }
        }
        if absolute_byte == cursor_byte && self.row < self.height {
            self.cursor = Some((self.column, self.row));
        }
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
    starts: Vec<usize>,
    next: usize,
    active: Vec<usize>,
}

impl<'a> DecorationResolver<'a> {
    fn new(decorations: &'a [DecorationSpan]) -> Self {
        let mut starts = (0..decorations.len()).collect::<Vec<_>>();
        starts.sort_by_key(|index| (decorations[*index].range.start, *index));
        Self {
            decorations,
            starts,
            next: 0,
            active: Vec::new(),
        }
    }

    fn style(&mut self, range: Range<usize>) -> CellStyle {
        self.active
            .retain(|index| self.decorations[*index].range.end > range.start);
        while let Some(index) = self.starts.get(self.next).copied()
            && self.decorations[index].range.start < range.end
        {
            self.active.push(index);
            self.next += 1;
        }
        decoration_style_from_indices(self.decorations, self.active.iter().copied(), range)
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
        let label = row_lines.get(index).copied().flatten().map_or_else(
            || " ".repeat(width),
            |line| {
                let number = if relative && line != cursor_line {
                    line.abs_diff(cursor_line)
                } else {
                    line.saturating_add(1)
                };
                format!("{number:>field$} ", field = width.saturating_sub(1))
            },
        );
        let style = row_lines
            .get(index)
            .copied()
            .flatten()
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
        let mut prefix = row_from_text(&label, width, style, 1);
        prefix.cells.append(&mut row.cells);
        *row = prefix;
    }
}

fn decoration_style(decorations: &[DecorationSpan], range: Range<usize>) -> CellStyle {
    decoration_style_from_indices(decorations, 0..decorations.len(), range)
}

fn decoration_style_from_indices(
    decorations: &[DecorationSpan],
    indices: impl IntoIterator<Item = usize>,
    range: Range<usize>,
) -> CellStyle {
    let mut style = CellStyle::default();
    let mut foreground_priority = None::<(u32, usize)>;
    let mut background_priority = None::<(u32, usize)>;
    for order in indices {
        let decoration = &decorations[order];
        if decoration.range.start >= range.end || range.start >= decoration.range.end {
            continue;
        }
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
    style
}

fn escape_grapheme(grapheme: &str) -> Vec<String> {
    if grapheme.chars().all(|character| !character.is_control()) {
        return vec![grapheme.to_owned()];
    }
    let mut escaped = Vec::new();
    for character in grapheme.chars() {
        match character {
            '\u{00}' => escaped.extend(["^".to_owned(), "@".to_owned()]),
            '\u{01}'..='\u{1a}' => {
                escaped.push("^".to_owned());
                let display = char::from_u32(u32::from(character) + 64).unwrap_or('?');
                escaped.push(display.to_string());
            }
            '\u{1b}' => escaped.extend(["^".to_owned(), "[".to_owned()]),
            '\u{1c}'..='\u{1f}' => {
                escaped.push("^".to_owned());
                let display = char::from_u32(u32::from(character) + 64).unwrap_or('?');
                escaped.push(display.to_string());
            }
            '\u{7f}' => escaped.extend(["^".to_owned(), "?".to_owned()]),
            _ => escaped.push(character.to_string()),
        }
    }
    escaped
}

fn row_from_text(text: &str, width: usize, style: CellStyle, tab_width: usize) -> CellRow {
    let mut row = CellRow::default();
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
    fn outside(
        decorations: &[DecorationSpan],
        start: usize,
        end: usize,
        mapped_end: usize,
    ) -> Option<Vec<DecorationSpan>> {
        let mut result = Vec::with_capacity(decorations.len());
        for decoration in decorations {
            if decoration.range.end <= start {
                result.push(decoration.clone());
            } else if decoration.range.start >= end {
                let mut decoration = decoration.clone();
                decoration.range.start = shift_after_line(decoration.range.start, end, mapped_end)?;
                decoration.range.end = shift_after_line(decoration.range.end, end, mapped_end)?;
                result.push(decoration);
            } else if decoration.range.start < start || decoration.range.end > end {
                return None;
            }
        }
        Some(result)
    }

    let Some(mapped_old) = outside(old, change.old_start, change.old_end, change.new_end) else {
        return false;
    };
    let Some(new_outside) = outside(new, change.new_start, change.new_end, change.new_end) else {
        return false;
    };
    mapped_old == new_outside
}

fn shift_after_line(byte: usize, old_end: usize, new_end: usize) -> Option<usize> {
    if new_end >= old_end {
        byte.checked_add(new_end - old_end)
    } else {
        byte.checked_sub(old_end - new_end)
    }
}

fn visual_rows_between_lines(
    text: &FrameText,
    start_line: usize,
    end_line: usize,
    width: usize,
    tab_width: usize,
) -> usize {
    (start_line..end_line)
        .map(|line| {
            let start = text.byte_of_line(line);
            let mut end = text.byte_of_line(line.saturating_add(1));
            if end > start && text.slice(end - 1..end).as_ref() == "\n" {
                end -= 1;
            }
            visual_line_rows(&text.slice(start..end), width, tab_width)
        })
        .sum()
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

fn byte_of_line(text: &str, line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    text.match_indices('\n')
        .nth(line - 1)
        .map_or(text.len(), |(offset, _)| offset + 1)
}

#[must_use]
pub fn diff(previous: Option<&DesiredGrid>, desired: &DesiredGrid) -> Vec<TerminalPatch> {
    let mut patches = vec![TerminalPatch::ShowCursor(false)];
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
            let mut style = None;
            for cell in &row.cells {
                if style != Some(cell.style) {
                    style = Some(cell.style);
                    patches.push(TerminalPatch::SetStyle(cell.style));
                }
                patches.push(TerminalPatch::Put(cell.clone()));
            }
            let clear_style = row
                .cells
                .last()
                .map_or_else(CellStyle::default, |cell| cell.style);
            patches.push(TerminalPatch::SetStyle(clear_style));
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
        let base = layout.desired_grid(&EngineFrame {
            text: "x\nx\nx\nx\n".into(),
            cursor_byte: 0,
        });
        let popup = TextPopup {
            title: "hover".into(),
            text: "a".repeat(80).into(),
            scroll: 0,
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
