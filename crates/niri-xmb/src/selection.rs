use crate::model::{Snapshot, WorkspaceModel};
use crate::render::SelectionVisual;
use std::collections::HashMap;

/// Selection staged while the popup is open.
#[derive(Debug, Clone, Default)]
pub(crate) struct Selection {
    output: String,
    workspace: usize,
    window: usize,
    /// Stable workspace identity for `workspace`.
    workspace_id: Option<u64>,
    /// Stable window identity for `window`.
    window_id: Option<u64>,
    /// Staged window identity for each workspace visited in this session.
    visited: HashMap<u64, u64>,
}

impl Selection {
    /// End the staging session. The next popup starts from live state again.
    pub(crate) fn clear_session(&mut self) {
        self.visited.clear();
    }

    /// Move selection ownership to an output, then select its active workspace.
    pub(crate) fn select_output(&mut self, output: String, snapshot: &Snapshot) {
        self.output = output;
        select_active_workspace(self, snapshot);
    }

    /// Record the current position so returning to this workspace restores it.
    fn remember(&mut self, snapshot: &Snapshot) {
        if let (Some(workspace), Some(window)) = (
            selected_workspace(self, snapshot),
            selected_window(self, snapshot),
        ) {
            self.visited.insert(workspace, window);
        }
    }

    /// Select the remembered window, or the window Niri would focus.
    fn entry_index(&self, workspace: &WorkspaceModel) -> usize {
        match self.visited.get(&workspace.id) {
            Some(remembered) => workspace
                .windows()
                .position(|window| window.id == *remembered)
                .unwrap_or_else(|| workspace.active_index()),
            _ => workspace.active_index(),
        }
    }
}

pub(crate) fn normalize(selection: &mut Selection, snapshot: &Snapshot) {
    if selection.output.is_empty() || !snapshot.outputs.contains_key(&selection.output) {
        selection.output = snapshot
            .focused_output()
            .or_else(|| snapshot.outputs.keys().next().map(String::as_str))
            .unwrap_or_default()
            .to_string();
    }
    if let Some(output) = snapshot.outputs.get(&selection.output) {
        if let Some(workspace_id) = selection.workspace_id
            && let Some(index) = output
                .workspaces
                .iter()
                .position(|workspace| workspace.id == workspace_id)
        {
            selection.workspace = index;
        }
        selection.workspace = selection
            .workspace
            .min(output.workspaces.len().saturating_sub(1));
        let Some(workspace) = output.workspaces.get(selection.workspace) else {
            selection.workspace_id = None;
            selection.window_id = None;
            selection.window = 0;
            return;
        };
        selection.workspace_id = Some(workspace.id);
        if let Some(window_id) = selection.window_id
            && let Some(index) = workspace
                .windows()
                .position(|window| window.id == window_id)
        {
            selection.window = index;
            return;
        }
        selection.window = selection
            .window
            .min(workspace.windows().count().saturating_sub(1));
        selection.window_id = workspace
            .windows()
            .nth(selection.window)
            .map(|window| window.id);
    }
}

pub(crate) fn reset(selection: &mut Selection, snapshot: &Snapshot) {
    selection.clear_session();
    selection.output = snapshot
        .focused_output()
        .or_else(|| snapshot.outputs.keys().next().map(String::as_str))
        .unwrap_or_default()
        .to_string();
    selection.workspace_id = None;
    selection.window_id = None;
    select_active_workspace(selection, snapshot);
}

pub(crate) fn select_active_workspace(selection: &mut Selection, snapshot: &Snapshot) {
    let Some(output) = snapshot.outputs.get(&selection.output) else {
        selection.workspace = 0;
        selection.window = 0;
        selection.workspace_id = None;
        selection.window_id = None;
        return;
    };
    selection.workspace = output
        .workspaces
        .iter()
        .position(|workspace| workspace.focused)
        .or_else(|| {
            output
                .workspaces
                .iter()
                .position(|workspace| workspace.active)
        })
        .unwrap_or(0);
    if let Some(workspace) = output.workspaces.get(selection.workspace) {
        selection.workspace_id = Some(workspace.id);
        selection.window = selection.entry_index(workspace);
        selection.window_id = workspace
            .windows()
            .nth(selection.window)
            .map(|window| window.id);
    } else {
        selection.workspace_id = None;
        selection.window = 0;
        selection.window_id = None;
    }
}

pub(crate) fn move_output(selection: &mut Selection, snapshot: &Snapshot, delta: i32) {
    if snapshot.outputs.is_empty() {
        return;
    }
    let outputs = snapshot.outputs.keys().collect::<Vec<_>>();
    let current = outputs
        .iter()
        .position(|output| output.as_str() == selection.output)
        .unwrap_or(0);
    selection.output = outputs[wrap_index(current, delta, outputs.len())].to_string();
    select_active_workspace(selection, snapshot);
}

pub(crate) fn move_within(
    selection: &mut Selection,
    snapshot: &Snapshot,
    workspace_delta: i32,
    window_delta: i32,
) {
    normalize(selection, snapshot);
    let Some(output) = snapshot.outputs.get(&selection.output) else {
        return;
    };
    if workspace_delta != 0 && !output.workspaces.is_empty() {
        selection.remember(snapshot);
        selection.workspace = wrap_index(
            selection.workspace,
            workspace_delta,
            output.workspaces.len(),
        );
        if let Some(workspace) = output.workspaces.get(selection.workspace) {
            selection.workspace_id = Some(workspace.id);
            selection.window = selection.entry_index(workspace);
            selection.window_id = workspace
                .windows()
                .nth(selection.window)
                .map(|window| window.id);
        }
    }
    if window_delta != 0 {
        let count = output
            .workspaces
            .get(selection.workspace)
            .map(|workspace| workspace.windows().count())
            .unwrap_or(0);
        if count > 0 {
            selection.window = wrap_index(selection.window, window_delta, count);
            selection.window_id = output
                .workspaces
                .get(selection.workspace)
                .and_then(|workspace| workspace.windows().nth(selection.window))
                .map(|window| window.id);
            selection.remember(snapshot);
        }
    }
}

pub(crate) fn wrap_index(current: usize, delta: i32, length: usize) -> usize {
    (current as i32 + delta).rem_euclid(length as i32) as usize
}

pub(crate) fn selected_workspace(selection: &Selection, snapshot: &Snapshot) -> Option<u64> {
    let output = snapshot.outputs.get(&selection.output)?;
    selection
        .workspace_id
        .filter(|id| {
            output
                .workspaces
                .iter()
                .any(|workspace| workspace.id == *id)
        })
        .or_else(|| {
            output
                .workspaces
                .get(selection.workspace)
                .map(|workspace| workspace.id)
        })
}

pub(crate) fn selected_window(selection: &Selection, snapshot: &Snapshot) -> Option<u64> {
    let workspace_id = selected_workspace(selection, snapshot)?;
    let workspace = snapshot.workspace(workspace_id)?;
    selection
        .window_id
        .filter(|id| workspace.windows().any(|window| window.id == *id))
        .or_else(|| {
            workspace
                .windows()
                .nth(selection.window)
                .map(|window| window.id)
        })
}

pub(crate) fn visual(selection: &Selection, snapshot: &Snapshot) -> Option<SelectionVisual> {
    Some(SelectionVisual {
        output: selection.output.clone(),
        workspace_id: selected_workspace(selection, snapshot)?,
        window_id: selected_window(selection, snapshot),
        anchors: selection.visited.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{OutputModel, WindowModel};
    use std::collections::BTreeMap;

    fn window(id: u64, column: usize) -> WindowModel {
        WindowModel {
            id,
            workspace_id: 0,
            app_id: "app".into(),
            title: String::new(),
            focused: false,
            urgent: false,
            floating: false,
            column: Some(column),
            tile_width: 100.0,
            tile_height: 100.0,
            x: None,
            y: None,
        }
    }

    fn workspace(id: u64, count: usize, active: usize) -> WorkspaceModel {
        let mut columns = BTreeMap::new();
        for index in 0..count {
            columns
                .entry(index)
                .or_insert_with(BTreeMap::new)
                .insert(0, window(id * 100 + index as u64, index));
        }
        WorkspaceModel {
            id,
            index: id as u8,
            name: None,
            active: id == 1,
            focused: id == 1,
            urgent: false,
            active_window: Some(id * 100 + active as u64),
            columns,
            floating: Vec::new(),
        }
    }

    fn snapshot() -> Snapshot {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "DP-1".to_string(),
            OutputModel {
                name: "DP-1".into(),
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
                workspaces: vec![workspace(1, 4, 2), workspace(2, 3, 1), workspace(3, 5, 4)],
            },
        );
        Snapshot {
            outputs,
            focused_window: None,
            overview_open: false,
            generation: 1,
        }
    }

    #[test]
    fn moving_between_workspaces_lands_on_active_window() {
        let snapshot = snapshot();
        let mut selection = Selection {
            output: "DP-1".into(),
            workspace: 0,
            window: 2,
            ..Selection::default()
        };
        move_within(&mut selection, &snapshot, 1, 0);
        assert_eq!((selection.workspace, selection.window), (1, 1));
        move_within(&mut selection, &snapshot, 1, 0);
        assert_eq!((selection.workspace, selection.window), (2, 4));
        move_within(&mut selection, &snapshot, 1, 0);
        assert_eq!((selection.workspace, selection.window), (0, 2));
    }

    #[test]
    fn vertical_movement_matches_render_anchor() {
        let snapshot = snapshot();
        let output = &snapshot.outputs["DP-1"];
        let mut selection = Selection {
            output: "DP-1".into(),
            ..Selection::default()
        };
        for _ in 0..output.workspaces.len() * 2 {
            let before = visual(&selection, &snapshot).expect("selection");
            move_within(&mut selection, &snapshot, 1, 0);
            let arrived = &output.workspaces[selection.workspace];
            assert_eq!(selection.window, before.anchor(arrived));
        }
    }

    #[test]
    fn navigation_restores_staged_window() {
        let snapshot = snapshot();
        let mut selection = Selection {
            output: "DP-1".into(),
            ..Selection::default()
        };
        move_within(&mut selection, &snapshot, 0, 1);
        move_within(&mut selection, &snapshot, 0, 1);
        move_within(&mut selection, &snapshot, 1, 0);
        move_within(&mut selection, &snapshot, -1, 0);
        assert_eq!((selection.workspace, selection.window), (0, 2));
    }

    #[test]
    fn reset_starts_from_live_state() {
        let snapshot = snapshot();
        let mut selection = Selection {
            output: "DP-1".into(),
            ..Selection::default()
        };
        move_within(&mut selection, &snapshot, 0, 1);
        reset(&mut selection, &snapshot);
        assert_eq!((selection.workspace, selection.window), (0, 2));
        assert!(selection.visited.is_empty());
    }

    #[test]
    fn stale_remembered_window_falls_back_to_active_window() {
        let snapshot = snapshot();
        let workspace = &snapshot.outputs["DP-1"].workspaces[1];
        let mut selection = Selection::default();
        selection.visited.insert(workspace.id, 99);
        assert_eq!(selection.entry_index(workspace), workspace.active_index());
    }

    #[test]
    fn selection_keeps_window_identity_after_reorder() {
        let mut snapshot = snapshot();
        let mut selection = Selection {
            output: "DP-1".into(),
            workspace: 0,
            window: 2,
            ..Selection::default()
        };
        normalize(&mut selection, &snapshot);
        assert_eq!(selected_window(&selection, &snapshot), Some(102));

        let workspace = &mut snapshot.outputs.get_mut("DP-1").unwrap().workspaces[0];
        let moved = workspace.columns.remove(&2).expect("third column");
        workspace.columns.insert(5, moved);
        normalize(&mut selection, &snapshot);

        assert_eq!(selection.window, 3);
        assert_eq!(selected_window(&selection, &snapshot), Some(102));
    }

    #[test]
    fn horizontal_movement_wraps_one_window_at_a_time() {
        let snapshot = snapshot();
        let mut selection = Selection {
            output: "DP-1".into(),
            workspace: 1,
            window: 1,
            ..Selection::default()
        };
        move_within(&mut selection, &snapshot, 0, 1);
        assert_eq!((selection.workspace, selection.window), (1, 2));
        move_within(&mut selection, &snapshot, 0, 1);
        assert_eq!((selection.workspace, selection.window), (1, 0));
    }
}
