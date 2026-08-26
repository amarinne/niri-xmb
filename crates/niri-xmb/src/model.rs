use niri_ipc::{Output, Window, WindowLayout, Workspace};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub outputs: BTreeMap<String, OutputModel>,
    pub focused_window: Option<u64>,
    /// Whether Niri's own overview is on screen.
    pub overview_open: bool,
    pub generation: u64,
    /// Counts changes that alter what is written on screen without moving
    /// anything: window titles today. Split from `generation` so a title
    /// churning at 10 Hz cannot drive relayouts and hit-test rebuilds,
    /// while still reaching consumers that display it.
    pub content_revision: u64,
}

impl Snapshot {
    pub fn focused_output(&self) -> Option<&str> {
        self.outputs
            .values()
            .find(|output| output.workspaces.iter().any(|workspace| workspace.focused))
            .map(|output| output.name.as_str())
    }

    pub fn window(&self, id: u64) -> Option<&WindowModel> {
        self.outputs
            .values()
            .flat_map(|output| &output.workspaces)
            .flat_map(|workspace| workspace.windows())
            .find(|window| window.id == id)
    }

    pub fn workspace(&self, id: u64) -> Option<&WorkspaceModel> {
        self.outputs
            .values()
            .flat_map(|output| &output.workspaces)
            .find(|workspace| workspace.id == id)
    }
}

#[derive(Debug, Clone)]
pub struct OutputModel {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub workspaces: Vec<WorkspaceModel>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceModel {
    pub id: u64,
    pub index: u8,
    pub name: Option<String>,
    pub active: bool,
    pub focused: bool,
    pub urgent: bool,
    /// Window Niri would focus when this workspace is activated.
    pub active_window: Option<u64>,
    pub columns: BTreeMap<usize, BTreeMap<usize, WindowModel>>,
    pub floating: Vec<WindowModel>,
}

/// Visual position of a window inside the XMB canvas.
///
/// Niri columns run horizontally. Windows joined into the same column use the
/// same `column` and differ by `row`, so the renderer can preserve the real
/// tiling topology instead of flattening every window into one ribbon.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowSlot {
    pub column: usize,
    pub row: usize,
}

impl WorkspaceModel {
    pub fn windows(&self) -> impl Iterator<Item = &WindowModel> {
        self.columns
            .values()
            .flat_map(|column| column.values())
            .chain(self.floating.iter())
    }

    /// Position of [`WorkspaceModel::active_window`] within [`WorkspaceModel::windows`].
    ///
    /// The popup parks this window on the focal point, so the focal column
    /// always shows what activating the workspace would land on.
    pub fn active_index(&self) -> usize {
        self.active_window
            .and_then(|active| self.windows().position(|window| window.id == active))
            .unwrap_or(0)
    }

    /// Visual column/row occupied by the window at flattened selection
    /// `index`. Floating windows each receive their own column after the tiled
    /// columns.
    pub fn window_slot_at(&self, index: usize) -> Option<WindowSlot> {
        let mut current = 0;
        for (column, rows) in self.columns.values().enumerate() {
            for row in 0..rows.len() {
                if current == index {
                    return Some(WindowSlot { column, row });
                }
                current += 1;
            }
        }
        self.floating
            .get(index.checked_sub(current)?)
            .map(|_| WindowSlot {
                column: self.columns.len() + index - current,
                row: 0,
            })
    }

    pub fn window_slot(&self, id: u64) -> Option<WindowSlot> {
        self.windows()
            .position(|window| window.id == id)
            .and_then(|index| self.window_slot_at(index))
    }

    pub fn active_slot(&self) -> WindowSlot {
        self.active_window
            .and_then(|id| self.window_slot(id))
            .unwrap_or_default()
    }

    pub fn max_stack(&self) -> usize {
        self.columns
            .values()
            .map(BTreeMap::len)
            .max()
            .unwrap_or(1)
            .max(1)
    }

    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.index.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct WindowModel {
    pub id: u64,
    pub workspace_id: u64,
    pub app_id: String,
    pub title: String,
    pub focused: bool,
    pub urgent: bool,
    pub floating: bool,
    pub column: Option<usize>,
    pub tile_width: f64,
    pub tile_height: f64,
    pub x: Option<f64>,
    pub y: Option<f64>,
}

#[derive(Default)]
pub struct NiriState {
    outputs: HashMap<String, Output>,
    workspaces: HashMap<u64, Workspace>,
    windows: HashMap<u64, Window>,
    overview_open: bool,
    generation: u64,
    content_revision: u64,
}

impl NiriState {
    pub fn replace_outputs(&mut self, outputs: HashMap<String, Output>) {
        self.outputs = outputs;
        self.generation += 1;
    }

    pub fn replace_workspaces(&mut self, workspaces: Vec<Workspace>) {
        self.workspaces = workspaces
            .into_iter()
            .map(|workspace| (workspace.id, workspace))
            .collect();
        self.generation += 1;
    }

    pub fn replace_windows(&mut self, windows: Vec<Window>) {
        self.windows = windows
            .into_iter()
            .map(|window| (window.id, window))
            .collect();
        self.generation += 1;
    }

    pub fn workspace_mut(&mut self, id: u64) -> Option<&mut Workspace> {
        self.workspaces.get_mut(&id)
    }

    pub fn set_workspace_active_window(&mut self, id: u64, active_window_id: Option<u64>) {
        let Some(workspace) = self.workspaces.get_mut(&id) else {
            return;
        };
        if workspace.active_window_id != active_window_id {
            workspace.active_window_id = active_window_id;
            self.generation += 1;
        }
    }

    pub fn activate_workspace(&mut self, id: u64, focused: bool) {
        let Some(output) = self
            .workspaces
            .get(&id)
            .and_then(|workspace| workspace.output.clone())
        else {
            return;
        };

        for workspace in self.workspaces.values_mut() {
            if workspace.output.as_deref() == Some(output.as_str()) {
                workspace.is_active = false;
            }
            if focused {
                workspace.is_focused = false;
            }
        }
        if let Some(workspace) = self.workspaces.get_mut(&id) {
            workspace.is_active = true;
            workspace.is_focused = focused;
        }
        self.generation += 1;
    }

    pub fn window_mut(&mut self, id: u64) -> Option<&mut Window> {
        self.windows.get_mut(&id)
    }

    pub fn set_window_layout(&mut self, id: u64, layout: WindowLayout) -> bool {
        let Some(window) = self.windows.get_mut(&id) else {
            return false;
        };
        if window.layout == layout {
            return false;
        }
        window.layout = layout;
        self.generation += 1;
        true
    }

    pub fn remove_window(&mut self, id: u64) {
        self.windows.remove(&id);
        self.generation += 1;
    }

    /// Store the latest window metadata and report whether anything represented
    /// by the XMB changed. Titles, PIDs, and focus timestamps can update many
    /// times per second without changing the workspace map, so they must not
    /// drive snapshots and redraws.
    pub fn upsert_window(&mut self, window: Window) -> bool {
        let mut changed = self.windows.get(&window.id).is_none_or(|previous| {
            previous.app_id != window.app_id
                || previous.workspace_id != window.workspace_id
                || previous.is_focused != window.is_focused
                || previous.is_floating != window.is_floating
                || previous.is_urgent != window.is_urgent
                || previous.layout != window.layout
        });
        // Titles move without anything moving. Tracked separately so they
        // reach the navigator and the stream without forcing a relayout.
        let content_changed = self
            .windows
            .get(&window.id)
            .is_some_and(|previous| previous.title != window.title);
        if window.is_focused {
            for other in self.windows.values_mut() {
                if other.id != window.id && other.is_focused {
                    other.is_focused = false;
                    changed = true;
                }
            }
        }
        self.windows.insert(window.id, window);
        if changed {
            self.generation += 1;
        }
        if content_changed {
            self.content_revision += 1;
        }
        // Either kind of change is worth publishing; only `changed` means the
        // layout moved.
        changed || content_changed
    }

    pub fn set_overview_open(&mut self, open: bool) {
        if self.overview_open != open {
            self.overview_open = open;
            self.generation += 1;
        }
    }

    pub fn set_focused_window(&mut self, id: Option<u64>) {
        for window in self.windows.values_mut() {
            window.is_focused = Some(window.id) == id;
        }
        self.generation += 1;
    }

    pub fn snapshot(&self) -> Snapshot {
        let focused_window = self
            .windows
            .values()
            .find(|window| window.is_focused)
            .map(|window| window.id)
            .or_else(|| {
                self.workspaces
                    .values()
                    .find(|workspace| workspace.is_focused)
                    .and_then(|workspace| workspace.active_window_id)
                    .filter(|id| self.windows.contains_key(id))
            });
        let mut outputs = BTreeMap::new();

        for (name, output) in &self.outputs {
            let logical = output.logical;
            let (x, y, width, height) = logical
                .map(|logical| (logical.x, logical.y, logical.width, logical.height))
                .unwrap_or((0, 0, 1920, 1080));
            outputs.insert(
                name.clone(),
                OutputModel {
                    x,
                    y,
                    name: name.clone(),
                    width: i32::try_from(width).unwrap_or(i32::MAX),
                    height: i32::try_from(height).unwrap_or(i32::MAX),
                    workspaces: Vec::new(),
                },
            );
        }

        let mut workspaces: Vec<_> = self.workspaces.values().cloned().collect();
        workspaces.sort_by_key(|workspace| (workspace.output.clone(), workspace.idx));

        for workspace in workspaces {
            let Some(output_name) = workspace.output.clone() else {
                continue;
            };
            let Some(output) = outputs.get_mut(&output_name) else {
                continue;
            };

            let mut model = WorkspaceModel {
                id: workspace.id,
                index: workspace.idx,
                name: workspace.name.clone(),
                active: workspace.is_active,
                focused: workspace.is_focused,
                urgent: workspace.is_urgent,
                active_window: workspace.active_window_id,
                columns: BTreeMap::new(),
                floating: Vec::new(),
            };

            let mut workspace_windows: Vec<_> = self
                .windows
                .values()
                .filter(|window| window.workspace_id == Some(workspace.id))
                .cloned()
                .collect();
            workspace_windows.sort_by_key(|window| {
                window
                    .layout
                    .pos_in_scrolling_layout
                    .unwrap_or((usize::MAX, usize::MAX))
            });

            for window in workspace_windows {
                let position = window.layout.pos_in_scrolling_layout;
                let window_model = WindowModel {
                    id: window.id,
                    workspace_id: workspace.id,
                    app_id: window.app_id.unwrap_or_default(),
                    title: window.title.unwrap_or_default(),
                    focused: focused_window == Some(window.id),
                    urgent: window.is_urgent,
                    floating: window.is_floating,
                    column: position.map(|position| position.0),
                    tile_width: window.layout.tile_size.0,
                    tile_height: window.layout.tile_size.1,
                    x: window
                        .layout
                        .tile_pos_in_workspace_view
                        .map(|position| position.0),
                    y: window
                        .layout
                        .tile_pos_in_workspace_view
                        .map(|position| position.1),
                };

                if window_model.floating || position.is_none() {
                    model.floating.push(window_model);
                } else if let Some((column, row)) = position {
                    model
                        .columns
                        .entry(column)
                        .or_default()
                        .insert(row, window_model);
                }
            }

            output.workspaces.push(model);
        }

        Snapshot {
            outputs,
            focused_window,
            overview_open: self.overview_open,
            generation: self.generation,
            content_revision: self.content_revision,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_window() -> Window {
        Window {
            id: 10,
            title: Some("first title".into()),
            app_id: Some("org.example.App".into()),
            pid: Some(1000),
            workspace_id: Some(1),
            is_focused: true,
            is_floating: false,
            is_urgent: false,
            layout: niri_ipc::WindowLayout {
                pos_in_scrolling_layout: Some((1, 1)),
                tile_size: (800.0, 600.0),
                window_size: (780, 580),
                tile_pos_in_workspace_view: Some((0.0, 0.0)),
                window_offset_in_tile: (10.0, 10.0),
            },
            focus_timestamp: None,
        }
    }

    /// A title change must reach consumers without being mistaken for a
    /// layout change: the navigator draws titles, but a title churning at
    /// 10 Hz must not drive relayouts and hit-test rebuilds.
    #[test]
    fn title_only_window_update_bumps_content_not_layout() {
        let mut state = NiriState::default();
        assert!(state.upsert_window(test_window()));
        let generation = state.snapshot().generation;
        let content = state.snapshot().content_revision;

        let mut update = test_window();
        update.title = Some("spinner changed".into());
        update.pid = Some(2000);
        // Worth publishing...
        assert!(state.upsert_window(update));

        // ...but not a layout change.
        assert_eq!(state.snapshot().generation, generation);
        assert_eq!(state.snapshot().content_revision, content + 1);
        assert_eq!(
            state
                .windows
                .get(&10)
                .and_then(|window| window.title.as_deref()),
            Some("spinner changed")
        );
        assert_eq!(
            state.windows.get(&10).and_then(|window| window.pid),
            Some(2000)
        );
    }

    /// A layout change must not be mistaken for a content change either.
    #[test]
    fn a_layout_change_leaves_content_revision_alone() {
        let mut state = NiriState::default();
        assert!(state.upsert_window(test_window()));
        let content = state.snapshot().content_revision;

        let mut update = test_window();
        update.is_focused = !update.is_focused;
        assert!(state.upsert_window(update));

        assert_eq!(state.snapshot().content_revision, content);
    }

    #[test]
    fn visually_relevant_window_updates_bump_generation() {
        let mut state = NiriState::default();
        assert!(state.upsert_window(test_window()));

        let mut app_update = test_window();
        app_update.app_id = Some("org.example.Other".into());
        let before = state.snapshot().generation;
        assert!(state.upsert_window(app_update.clone()));
        assert_eq!(state.snapshot().generation, before + 1);

        let mut workspace_update = app_update.clone();
        workspace_update.workspace_id = Some(2);
        let before = state.snapshot().generation;
        assert!(state.upsert_window(workspace_update.clone()));
        assert_eq!(state.snapshot().generation, before + 1);

        let mut layout_update = workspace_update;
        layout_update.layout.pos_in_scrolling_layout = Some((2, 1));
        let before = state.snapshot().generation;
        assert!(state.upsert_window(layout_update));
        assert_eq!(state.snapshot().generation, before + 1);
    }

    #[test]
    fn layout_only_update_bumps_generation() {
        let mut state = NiriState::default();
        assert!(state.upsert_window(test_window()));
        let before = state.snapshot().generation;

        let mut layout = test_window().layout;
        layout.pos_in_scrolling_layout = Some((2, 1));
        assert!(state.set_window_layout(10, layout.clone()));
        assert_eq!(state.snapshot().generation, before + 1);

        assert!(!state.set_window_layout(10, layout));
        assert_eq!(state.snapshot().generation, before + 1);
    }

    #[test]
    fn overview_active_window_change_bumps_generation() {
        let mut state = NiriState::default();
        state.replace_workspaces(vec![Workspace {
            id: 1,
            idx: 1,
            name: None,
            output: Some("eDP-1".into()),
            is_urgent: false,
            is_active: true,
            is_focused: true,
            active_window_id: Some(10),
        }]);
        let before = state.snapshot().generation;

        state.set_workspace_active_window(1, Some(11));
        let after = state.snapshot().generation;

        assert_eq!(after, before + 1);
        assert_eq!(
            state
                .workspaces
                .get(&1)
                .and_then(|workspace| workspace.active_window_id),
            Some(11)
        );

        let stable = state.snapshot().generation;
        state.set_workspace_active_window(1, Some(11));
        assert_eq!(state.snapshot().generation, stable);
    }
}
