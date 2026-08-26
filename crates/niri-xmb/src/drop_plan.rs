use crate::model::{Snapshot, WindowModel};
use crate::niri::{JoinDirection, NiriCommand};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DropPoint {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DropTarget {
    Panel,
    Workspace {
        workspace_id: u64,
    },
    Canvas {
        workspace_id: u64,
        point: DropPoint,
    },
    Window {
        workspace_id: u64,
        window_id: u64,
        fraction: f32,
        canvas_point: Option<DropPoint>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropPolicy {
    pub allow_column_join: bool,
    pub empty_drop_floats: bool,
    pub floating_drop_tiles: bool,
    pub focus_after_drop: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropRequest {
    pub source_window_id: u64,
    pub target: DropTarget,
    pub policy: DropPolicy,
    pub preserved_focus: Option<u64>,
    pub restore_cursor: Option<(f64, f64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropPlan {
    pub base_generation: u64,
    pub source_window_id: u64,
    pub source_workspace_id: u64,
    pub source_joined: bool,
    pub destination: DropDestination,
    pub projection: DropProjection,
    pub confirmation: DropConfirmation,
    pub restore_focus: Option<u64>,
    pub restore_cursor: Option<(f64, f64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DropDestination {
    Tiled {
        workspace_id: u64,
        column: usize,
        join: Option<JoinDirection>,
    },
    Floating {
        workspace_id: u64,
        x: f64,
        y: f64,
        make_floating: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropProjection {
    Exact(Vec<ProjectedWorkspace>),
    Unsupported(ProjectionLimit),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionLimit {
    JoinNeedsLiveValidation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropConfirmation {
    ExactProjection,
    JoinedWith {
        workspace_id: u64,
        target_window_id: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedWorkspace {
    pub workspace_id: u64,
    pub columns: Vec<Vec<u64>>,
    pub floating: Vec<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropPlanError {
    MissingSource,
    MissingWorkspace,
    MissingTargetWindow,
    TargetWindowChangedWorkspace,
    SourceIsTarget,
    MissingOutput,
    MissingFloatingCanvas,
    InvalidCoordinate,
    PanelTarget,
}

impl std::fmt::Display for DropPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self {
            Self::MissingSource => "missing source window",
            Self::MissingWorkspace => "unknown workspace",
            Self::MissingTargetWindow => "missing target window",
            Self::TargetWindowChangedWorkspace => "target window changed workspace",
            Self::SourceIsTarget => "source window is the drop target",
            Self::MissingOutput => "no output hosts the target workspace",
            Self::MissingFloatingCanvas => "floating target lacks canvas position",
            Self::InvalidCoordinate => "invalid drop coordinate",
            Self::PanelTarget => "panel is not a drop target",
        };
        f.write_str(reason)
    }
}

impl DropTarget {
    /// A stable, title-free description for logs and diagnostics.
    pub fn describe(&self) -> String {
        match self {
            Self::Panel => "panel".to_string(),
            Self::Workspace { workspace_id } => format!("workspace {workspace_id}"),
            Self::Canvas {
                workspace_id,
                point,
            } => format!("canvas {workspace_id} at ({:.3}, {:.3})", point.x, point.y),
            Self::Window {
                workspace_id,
                window_id,
                fraction,
                ..
            } => format!("window {window_id} in workspace {workspace_id} at {fraction:.3}"),
        }
    }
}

impl DropConfirmation {
    /// The postcondition a transaction waits for, described without titles.
    pub fn describe(&self) -> String {
        match self {
            Self::ExactProjection => "exact layout projection".to_string(),
            Self::JoinedWith {
                workspace_id,
                target_window_id,
            } => format!("joined with window {target_window_id} in workspace {workspace_id}"),
        }
    }
}

impl DropPlan {
    pub fn into_command(self) -> NiriCommand {
        match self.destination {
            DropDestination::Tiled {
                workspace_id,
                column,
                join,
            } => NiriCommand::MoveTiled {
                window_id: self.source_window_id,
                source_workspace_id: self.source_workspace_id,
                workspace_id,
                column,
                join,
                source_joined: self.source_joined,
                restore_focus: self.restore_focus,
                restore_cursor: self.restore_cursor,
            },
            DropDestination::Floating {
                workspace_id,
                x,
                y,
                make_floating,
            } => NiriCommand::MoveFloating {
                window_id: self.source_window_id,
                source_workspace_id: self.source_workspace_id,
                workspace_id,
                x,
                y,
                make_floating,
                restore_focus: self.restore_focus,
                restore_cursor: self.restore_cursor,
            },
        }
    }

    pub fn postcondition_matches(&self, snapshot: &Snapshot) -> bool {
        match &self.confirmation {
            DropConfirmation::ExactProjection => {
                let DropProjection::Exact(expected) = &self.projection else {
                    return false;
                };
                expected.iter().all(|expected| {
                    match projected_workspace(snapshot, expected.workspace_id) {
                        Ok(current) => workspace_projection_matches(expected, &current),
                        // Niri destroys a workspace once it empties. A drop
                        // that takes the last window out therefore expects a
                        // workspace the live snapshot no longer reports, and
                        // absence satisfies an empty projection.
                        Err(DropPlanError::MissingWorkspace) => {
                            expected.columns.is_empty() && expected.floating.is_empty()
                        }
                        Err(_) => false,
                    }
                })
            }
            DropConfirmation::JoinedWith {
                workspace_id,
                target_window_id,
            } => {
                let Some(source) = snapshot.window(self.source_window_id) else {
                    return false;
                };
                let Some(target) = snapshot.window(*target_window_id) else {
                    return false;
                };
                source.workspace_id == *workspace_id
                    && target.workspace_id == *workspace_id
                    && !source.floating
                    && !target.floating
                    && source.column.is_some()
                    && source.column == target.column
            }
        }
    }
}

pub fn plan_drop(snapshot: &Snapshot, request: DropRequest) -> Result<DropPlan, DropPlanError> {
    if request
        .restore_cursor
        .is_some_and(|(x, y)| !x.is_finite() || !y.is_finite())
    {
        return Err(DropPlanError::InvalidCoordinate);
    }
    let source = snapshot
        .window(request.source_window_id)
        .ok_or(DropPlanError::MissingSource)?;
    let source_workspace_id = source.workspace_id;
    let source_joined = source.column.is_some_and(|column| {
        snapshot
            .workspace(source_workspace_id)
            .and_then(|workspace| workspace.columns.get(&column))
            .is_some_and(|windows| windows.len() > 1)
    });
    let restore_focus = if request.policy.focus_after_drop {
        None
    } else {
        request.preserved_focus.or(snapshot.focused_window)
    };

    let confirmation_target = request.target.clone();
    let destination = match request.target {
        DropTarget::Panel => return Err(DropPlanError::PanelTarget),
        DropTarget::Workspace { workspace_id } => {
            let workspace = snapshot
                .workspace(workspace_id)
                .ok_or(DropPlanError::MissingWorkspace)?;
            DropDestination::Tiled {
                workspace_id,
                column: workspace.columns.len() + 1,
                join: None,
            }
        }
        DropTarget::Canvas {
            workspace_id,
            point,
        } => {
            validate_point(point)?;
            let workspace = snapshot
                .workspace(workspace_id)
                .ok_or(DropPlanError::MissingWorkspace)?;
            if source.floating || request.policy.empty_drop_floats {
                floating_destination(snapshot, source, workspace_id, point, true)?
            } else {
                let insertion_slots = workspace.columns.len() + 1;
                let column =
                    (point.x.clamp(0.0, 1.0) * insertion_slots as f32).floor() as usize + 1;
                let column = column.min(insertion_slots);
                DropDestination::Tiled {
                    workspace_id,
                    column: adjust_same_workspace_index(
                        column,
                        source.column,
                        source_workspace_id == workspace_id,
                    ),
                    join: None,
                }
            }
        }
        DropTarget::Window {
            workspace_id,
            window_id,
            fraction,
            canvas_point,
        } => {
            if !fraction.is_finite() {
                return Err(DropPlanError::InvalidCoordinate);
            }
            if let Some(point) = canvas_point {
                validate_point(point)?;
            }
            if window_id == request.source_window_id {
                return Err(DropPlanError::SourceIsTarget);
            }
            snapshot
                .workspace(workspace_id)
                .ok_or(DropPlanError::MissingWorkspace)?;
            let target = snapshot
                .window(window_id)
                .ok_or(DropPlanError::MissingTargetWindow)?;
            if target.workspace_id != workspace_id {
                return Err(DropPlanError::TargetWindowChangedWorkspace);
            }
            if target.floating {
                let point = canvas_point.ok_or(DropPlanError::MissingFloatingCanvas)?;
                floating_destination(snapshot, source, workspace_id, point, true)?
            } else if source.floating && !request.policy.floating_drop_tiles {
                preserved_floating_destination(snapshot, source, workspace_id)?
            } else {
                let target_column = target.column.unwrap_or_else(|| {
                    snapshot
                        .workspace(workspace_id)
                        .map(|workspace| workspace.columns.len().max(1))
                        .unwrap_or(1)
                });
                let (column, join) = tiled_drop_position(
                    target_column,
                    fraction.clamp(0.0, 1.0),
                    request.policy.allow_column_join,
                );
                DropDestination::Tiled {
                    workspace_id,
                    column: adjust_same_workspace_index(
                        column,
                        source.column,
                        source_workspace_id == workspace_id,
                    ),
                    join,
                }
            }
        }
    };

    let projection = project_drop(
        snapshot,
        request.source_window_id,
        source_workspace_id,
        &destination,
    )?;
    let confirmation = match (&projection, confirmation_target) {
        (DropProjection::Exact(_), _) => DropConfirmation::ExactProjection,
        (
            DropProjection::Unsupported(ProjectionLimit::JoinNeedsLiveValidation),
            DropTarget::Window {
                workspace_id,
                window_id,
                ..
            },
        ) => DropConfirmation::JoinedWith {
            workspace_id,
            target_window_id: window_id,
        },
        _ => return Err(DropPlanError::MissingTargetWindow),
    };
    Ok(DropPlan {
        base_generation: snapshot.generation,
        source_window_id: request.source_window_id,
        source_workspace_id,
        source_joined,
        destination,
        projection,
        confirmation,
        restore_focus,
        restore_cursor: request.restore_cursor,
    })
}

fn workspace_projection_matches(
    expected: &ProjectedWorkspace,
    current: &ProjectedWorkspace,
) -> bool {
    if expected.workspace_id != current.workspace_id || expected.columns != current.columns {
        return false;
    }
    let mut expected_floating = expected.floating.clone();
    let mut current_floating = current.floating.clone();
    expected_floating.sort_unstable();
    current_floating.sort_unstable();
    expected_floating == current_floating
}

fn project_drop(
    snapshot: &Snapshot,
    source_window_id: u64,
    source_workspace_id: u64,
    destination: &DropDestination,
) -> Result<DropProjection, DropPlanError> {
    if matches!(destination, DropDestination::Tiled { join: Some(_), .. }) {
        return Ok(DropProjection::Unsupported(
            ProjectionLimit::JoinNeedsLiveValidation,
        ));
    }
    let target_workspace_id = match destination {
        DropDestination::Tiled { workspace_id, .. }
        | DropDestination::Floating { workspace_id, .. } => *workspace_id,
    };
    let mut source = projected_workspace(snapshot, source_workspace_id)?;
    remove_window(&mut source, source_window_id);

    if source_workspace_id == target_workspace_id {
        insert_destination(&mut source, source_window_id, destination);
        return Ok(DropProjection::Exact(vec![source]));
    }

    let mut target = projected_workspace(snapshot, target_workspace_id)?;
    remove_window(&mut target, source_window_id);
    insert_destination(&mut target, source_window_id, destination);
    Ok(DropProjection::Exact(vec![source, target]))
}

fn projected_workspace(
    snapshot: &Snapshot,
    workspace_id: u64,
) -> Result<ProjectedWorkspace, DropPlanError> {
    let workspace = snapshot
        .workspace(workspace_id)
        .ok_or(DropPlanError::MissingWorkspace)?;
    Ok(ProjectedWorkspace {
        workspace_id,
        columns: workspace
            .columns
            .values()
            .map(|rows| rows.values().map(|window| window.id).collect())
            .collect(),
        floating: workspace.floating.iter().map(|window| window.id).collect(),
    })
}

fn remove_window(workspace: &mut ProjectedWorkspace, window_id: u64) {
    for column in &mut workspace.columns {
        column.retain(|id| *id != window_id);
    }
    workspace.columns.retain(|column| !column.is_empty());
    workspace.floating.retain(|id| *id != window_id);
}

fn insert_destination(
    workspace: &mut ProjectedWorkspace,
    source_window_id: u64,
    destination: &DropDestination,
) {
    match destination {
        DropDestination::Tiled {
            column, join: None, ..
        } => {
            let index = column.saturating_sub(1).min(workspace.columns.len());
            workspace.columns.insert(index, vec![source_window_id]);
        }
        DropDestination::Floating { .. } => workspace.floating.push(source_window_id),
        DropDestination::Tiled { join: Some(_), .. } => {}
    }
}

fn validate_point(point: DropPoint) -> Result<(), DropPlanError> {
    if point.x.is_finite() && point.y.is_finite() {
        Ok(())
    } else {
        Err(DropPlanError::InvalidCoordinate)
    }
}

fn floating_destination(
    snapshot: &Snapshot,
    source: &WindowModel,
    workspace_id: u64,
    point: DropPoint,
    make_floating: bool,
) -> Result<DropDestination, DropPlanError> {
    let output =
        output_for_workspace(snapshot, workspace_id).ok_or(DropPlanError::MissingOutput)?;
    let center_x = point.x.clamp(0.0, 1.0) * output.width as f32;
    let center_y = point.y.clamp(0.0, 1.0) * output.height as f32;
    let x = (center_x as f64 - source.tile_width * 0.5)
        .clamp(0.0, (output.width as f64 - source.tile_width).max(0.0));
    let y = (center_y as f64 - source.tile_height * 0.5)
        .clamp(0.0, (output.height as f64 - source.tile_height).max(0.0));
    Ok(DropDestination::Floating {
        workspace_id,
        x,
        y,
        make_floating,
    })
}

fn preserved_floating_destination(
    snapshot: &Snapshot,
    source: &WindowModel,
    workspace_id: u64,
) -> Result<DropDestination, DropPlanError> {
    let source_output =
        output_for_workspace(snapshot, source.workspace_id).ok_or(DropPlanError::MissingOutput)?;
    let target_output =
        output_for_workspace(snapshot, workspace_id).ok_or(DropPlanError::MissingOutput)?;
    let normalized_x = source.x.unwrap_or(0.0) / source_output.width.max(1) as f64;
    let normalized_y = source.y.unwrap_or(0.0) / source_output.height.max(1) as f64;
    let x = (normalized_x * target_output.width as f64).clamp(
        0.0,
        (target_output.width as f64 - source.tile_width).max(0.0),
    );
    let y = (normalized_y * target_output.height as f64).clamp(
        0.0,
        (target_output.height as f64 - source.tile_height).max(0.0),
    );
    Ok(DropDestination::Floating {
        workspace_id,
        x,
        y,
        make_floating: false,
    })
}

fn output_for_workspace(
    snapshot: &Snapshot,
    workspace_id: u64,
) -> Option<&crate::model::OutputModel> {
    snapshot.outputs.values().find(|output| {
        output
            .workspaces
            .iter()
            .any(|workspace| workspace.id == workspace_id)
    })
}

fn tiled_drop_position(
    target_column: usize,
    fraction: f32,
    allow_join: bool,
) -> (usize, Option<JoinDirection>) {
    let target_index = target_column.max(1);
    if allow_join && (1.0 / 3.0..=2.0 / 3.0).contains(&fraction) {
        (target_index + 1, Some(JoinDirection::Left))
    } else if fraction < 0.5 {
        (target_index, None)
    } else {
        (target_index + 1, None)
    }
}

fn adjust_same_workspace_index(
    insertion_index: usize,
    source_column: Option<usize>,
    same_workspace: bool,
) -> usize {
    if same_workspace && source_column.is_some_and(|source| source < insertion_index) {
        insertion_index.saturating_sub(1).max(1)
    } else {
        insertion_index.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{OutputModel, Snapshot, WindowModel, WorkspaceModel};
    use std::collections::BTreeMap;

    fn window(id: u64, workspace_id: u64, column: Option<usize>, floating: bool) -> WindowModel {
        WindowModel {
            id,
            workspace_id,
            app_id: format!("app-{id}"),
            title: String::new(),
            focused: id == 11,
            urgent: false,
            floating,
            column,
            tile_width: 400.0,
            tile_height: 300.0,
            x: Some(200.0),
            y: Some(100.0),
        }
    }

    fn workspace(
        id: u64,
        tiled: &[(usize, Vec<WindowModel>)],
        floating: Vec<WindowModel>,
    ) -> WorkspaceModel {
        WorkspaceModel {
            id,
            index: id as u8,
            name: None,
            active: id == 1,
            focused: id == 1,
            urgent: false,
            active_window: tiled
                .first()
                .and_then(|(_, windows)| windows.first())
                .map(|window| window.id),
            columns: tiled
                .iter()
                .map(|(column, windows)| {
                    (
                        *column,
                        windows
                            .iter()
                            .enumerate()
                            .map(|(row, window)| (row + 1, window.clone()))
                            .collect(),
                    )
                })
                .collect(),
            floating,
        }
    }

    fn snapshot() -> Snapshot {
        let source = window(11, 1, Some(1), false);
        let joined = window(12, 1, Some(1), false);
        let target = window(13, 1, Some(2), false);
        let other = window(21, 2, Some(1), false);
        let floating = window(31, 1, None, true);
        Snapshot {
            outputs: BTreeMap::from([
                (
                    "left".into(),
                    OutputModel {
                        name: "left".into(),
                        x: 0,
                        y: 0,
                        width: 1600,
                        height: 900,
                        workspaces: vec![workspace(
                            1,
                            &[(1, vec![source, joined]), (2, vec![target])],
                            vec![floating],
                        )],
                    },
                ),
                (
                    "right".into(),
                    OutputModel {
                        name: "right".into(),
                        x: 1600,
                        y: 0,
                        width: 2400,
                        height: 1200,
                        workspaces: vec![workspace(2, &[(1, vec![other])], Vec::new())],
                    },
                ),
            ]),
            focused_window: Some(11),
            overview_open: false,
            generation: 1,
            content_revision: 0,
        }
    }

    fn policy() -> DropPolicy {
        DropPolicy {
            allow_column_join: false,
            empty_drop_floats: false,
            floating_drop_tiles: true,
            focus_after_drop: false,
        }
    }

    fn request(source_window_id: u64, target: DropTarget) -> DropRequest {
        DropRequest {
            source_window_id,
            target,
            policy: policy(),
            preserved_focus: Some(99),
            restore_cursor: Some((50.0, 60.0)),
        }
    }

    #[test]
    fn same_workspace_insertion_accounts_for_source_removal() {
        let plan = plan_drop(
            &snapshot(),
            request(
                11,
                DropTarget::Window {
                    workspace_id: 1,
                    window_id: 13,
                    fraction: 0.9,
                    canvas_point: None,
                },
            ),
        )
        .unwrap();
        assert_eq!(
            plan.destination,
            DropDestination::Tiled {
                workspace_id: 1,
                column: 2,
                join: None,
            }
        );
        assert!(plan.source_joined);
        assert_eq!(plan.base_generation, 1);
        assert_eq!(plan.restore_focus, Some(99));
        assert_eq!(
            plan.projection,
            DropProjection::Exact(vec![ProjectedWorkspace {
                workspace_id: 1,
                columns: vec![vec![12], vec![11], vec![13]],
                floating: vec![31],
            }])
        );
    }

    #[test]
    fn cross_workspace_insertion_keeps_one_based_index() {
        let plan = plan_drop(
            &snapshot(),
            request(
                11,
                DropTarget::Window {
                    workspace_id: 2,
                    window_id: 21,
                    fraction: 0.9,
                    canvas_point: None,
                },
            ),
        )
        .unwrap();
        assert_eq!(
            plan.destination,
            DropDestination::Tiled {
                workspace_id: 2,
                column: 2,
                join: None,
            }
        );
        assert_eq!(
            plan.projection,
            DropProjection::Exact(vec![
                ProjectedWorkspace {
                    workspace_id: 1,
                    columns: vec![vec![12], vec![13]],
                    floating: vec![31],
                },
                ProjectedWorkspace {
                    workspace_id: 2,
                    columns: vec![vec![21], vec![11]],
                    floating: Vec::new(),
                },
            ])
        );
    }

    #[test]
    fn join_policy_uses_center_zone() {
        let mut request = request(
            11,
            DropTarget::Window {
                workspace_id: 2,
                window_id: 21,
                fraction: 0.5,
                canvas_point: None,
            },
        );
        request.policy.allow_column_join = true;
        let plan = plan_drop(&snapshot(), request).unwrap();
        assert_eq!(
            plan.destination,
            DropDestination::Tiled {
                workspace_id: 2,
                column: 2,
                join: Some(JoinDirection::Left),
            }
        );
        assert_eq!(
            plan.projection,
            DropProjection::Unsupported(ProjectionLimit::JoinNeedsLiveValidation)
        );
    }

    #[test]
    fn join_zone_boundaries_are_inclusive() {
        for fraction in [1.0 / 3.0, 2.0 / 3.0] {
            let mut request = request(
                11,
                DropTarget::Window {
                    workspace_id: 2,
                    window_id: 21,
                    fraction,
                    canvas_point: None,
                },
            );
            request.policy.allow_column_join = true;
            let plan = plan_drop(&snapshot(), request).unwrap();
            assert!(matches!(
                plan.destination,
                DropDestination::Tiled {
                    join: Some(JoinDirection::Left),
                    ..
                }
            ));
        }
    }

    #[test]
    fn disabled_join_routes_center_after_target() {
        let plan = plan_drop(
            &snapshot(),
            request(
                11,
                DropTarget::Window {
                    workspace_id: 2,
                    window_id: 21,
                    fraction: 0.5,
                    canvas_point: None,
                },
            ),
        )
        .unwrap();
        assert_eq!(
            plan.destination,
            DropDestination::Tiled {
                workspace_id: 2,
                column: 2,
                join: None,
            }
        );
    }

    #[test]
    fn workspace_drop_appends_after_last_column() {
        let plan = plan_drop(
            &snapshot(),
            request(11, DropTarget::Workspace { workspace_id: 2 }),
        )
        .unwrap();
        assert_eq!(
            plan.destination,
            DropDestination::Tiled {
                workspace_id: 2,
                column: 2,
                join: None,
            }
        );
    }

    #[test]
    fn same_workspace_card_drop_preserves_append_quirk() {
        let plan = plan_drop(
            &snapshot(),
            request(11, DropTarget::Workspace { workspace_id: 1 }),
        )
        .unwrap();
        assert_eq!(
            plan.destination,
            DropDestination::Tiled {
                workspace_id: 1,
                column: 3,
                join: None,
            },
            "workspace-card drops append using the pre-removal column count"
        );
    }

    #[test]
    fn canvas_drop_uses_fractional_insertion_slot() {
        let plan = plan_drop(
            &snapshot(),
            request(
                11,
                DropTarget::Canvas {
                    workspace_id: 2,
                    point: DropPoint { x: 0.9, y: 0.5 },
                },
            ),
        )
        .unwrap();
        assert_eq!(
            plan.destination,
            DropDestination::Tiled {
                workspace_id: 2,
                column: 2,
                join: None,
            }
        );
    }

    #[test]
    fn projection_changes_only_affected_workspaces() {
        let mut snapshot = snapshot();
        snapshot
            .outputs
            .get_mut("right")
            .unwrap()
            .workspaces
            .push(workspace(
                3,
                &[(1, vec![window(41, 3, Some(1), false)])],
                Vec::new(),
            ));
        let plan = plan_drop(
            &snapshot,
            request(
                11,
                DropTarget::Window {
                    workspace_id: 2,
                    window_id: 21,
                    fraction: 0.9,
                    canvas_point: None,
                },
            ),
        )
        .unwrap();
        let DropProjection::Exact(workspaces) = plan.projection else {
            panic!("normal insertion must have an exact projection");
        };
        assert_eq!(workspaces.len(), 2);
        assert!(
            workspaces
                .iter()
                .all(|workspace| workspace.workspace_id != 3)
        );
        assert_eq!(
            snapshot.workspace(3).unwrap().windows().next().unwrap().id,
            41
        );
    }

    #[test]
    fn tiled_window_can_float_on_canvas() {
        let mut request = request(
            11,
            DropTarget::Canvas {
                workspace_id: 2,
                point: DropPoint { x: 0.5, y: 0.5 },
            },
        );
        request.policy.empty_drop_floats = true;
        let plan = plan_drop(&snapshot(), request).unwrap();
        assert_eq!(
            plan.destination,
            DropDestination::Floating {
                workspace_id: 2,
                x: 1000.0,
                y: 450.0,
                make_floating: true,
            }
        );
    }

    #[test]
    fn floating_window_keeps_normalized_position_when_tiling_is_disabled() {
        let mut request = request(
            31,
            DropTarget::Window {
                workspace_id: 2,
                window_id: 21,
                fraction: 0.25,
                canvas_point: None,
            },
        );
        request.policy.floating_drop_tiles = false;
        let plan = plan_drop(&snapshot(), request).unwrap();
        assert_eq!(
            plan.destination,
            DropDestination::Floating {
                workspace_id: 2,
                x: 300.0,
                y: 133.33333333333331,
                make_floating: false,
            }
        );
    }

    #[test]
    fn focus_after_drop_omits_focus_restoration() {
        let mut request = request(11, DropTarget::Workspace { workspace_id: 2 });
        request.policy.focus_after_drop = true;
        let plan = plan_drop(&snapshot(), request).unwrap();
        assert_eq!(plan.restore_focus, None);
    }

    #[test]
    fn stale_or_missing_inputs_are_rejected() {
        assert_eq!(
            plan_drop(
                &snapshot(),
                request(999, DropTarget::Workspace { workspace_id: 2 })
            ),
            Err(DropPlanError::MissingSource)
        );
        assert_eq!(
            plan_drop(
                &snapshot(),
                request(11, DropTarget::Workspace { workspace_id: 999 })
            ),
            Err(DropPlanError::MissingWorkspace)
        );
        assert_eq!(
            plan_drop(
                &snapshot(),
                request(
                    11,
                    DropTarget::Window {
                        workspace_id: 2,
                        window_id: 999,
                        fraction: 0.5,
                        canvas_point: None,
                    }
                )
            ),
            Err(DropPlanError::MissingTargetWindow)
        );
        assert_eq!(
            plan_drop(
                &snapshot(),
                request(
                    11,
                    DropTarget::Window {
                        workspace_id: 2,
                        window_id: 13,
                        fraction: 0.5,
                        canvas_point: None,
                    }
                )
            ),
            Err(DropPlanError::TargetWindowChangedWorkspace)
        );
    }

    #[test]
    fn source_target_and_floating_target_without_canvas_are_rejected() {
        assert_eq!(
            plan_drop(
                &snapshot(),
                request(
                    11,
                    DropTarget::Window {
                        workspace_id: 1,
                        window_id: 11,
                        fraction: 0.5,
                        canvas_point: None,
                    }
                )
            ),
            Err(DropPlanError::SourceIsTarget)
        );
        assert_eq!(
            plan_drop(
                &snapshot(),
                request(
                    11,
                    DropTarget::Window {
                        workspace_id: 1,
                        window_id: 31,
                        fraction: 0.5,
                        canvas_point: None,
                    }
                )
            ),
            Err(DropPlanError::MissingFloatingCanvas)
        );
    }

    #[test]
    fn nonfinite_coordinates_are_rejected() {
        assert_eq!(
            plan_drop(
                &snapshot(),
                request(
                    11,
                    DropTarget::Canvas {
                        workspace_id: 2,
                        point: DropPoint {
                            x: f32::NAN,
                            y: 0.5,
                        },
                    }
                )
            ),
            Err(DropPlanError::InvalidCoordinate)
        );
        assert_eq!(
            plan_drop(
                &snapshot(),
                request(
                    11,
                    DropTarget::Window {
                        workspace_id: 2,
                        window_id: 21,
                        fraction: f32::INFINITY,
                        canvas_point: None,
                    }
                )
            ),
            Err(DropPlanError::InvalidCoordinate)
        );
        let mut cursor = request(11, DropTarget::Workspace { workspace_id: 2 });
        cursor.restore_cursor = Some((f64::NAN, 1.0));
        assert_eq!(
            plan_drop(&snapshot(), cursor),
            Err(DropPlanError::InvalidCoordinate)
        );
    }

    #[test]
    fn plan_converts_to_current_composite_executor_command() {
        let plan = plan_drop(
            &snapshot(),
            request(11, DropTarget::Workspace { workspace_id: 2 }),
        )
        .unwrap();
        assert!(matches!(
            plan.into_command(),
            NiriCommand::MoveTiled {
                window_id: 11,
                source_workspace_id: 1,
                workspace_id: 2,
                column: 2,
                join: None,
                source_joined: true,
                restore_focus: Some(99),
                restore_cursor: Some((50.0, 60.0)),
            }
        ));
    }

    #[test]
    fn every_rejection_has_a_stable_message() {
        assert_eq!(
            DropPlanError::MissingSource.to_string(),
            "missing source window"
        );
        assert_eq!(
            DropPlanError::MissingWorkspace.to_string(),
            "unknown workspace"
        );
        assert_eq!(
            DropPlanError::MissingTargetWindow.to_string(),
            "missing target window"
        );
        assert_eq!(
            DropPlanError::TargetWindowChangedWorkspace.to_string(),
            "target window changed workspace"
        );
        assert_eq!(
            DropPlanError::SourceIsTarget.to_string(),
            "source window is the drop target"
        );
        assert_eq!(
            DropPlanError::MissingOutput.to_string(),
            "no output hosts the target workspace"
        );
        assert_eq!(
            DropPlanError::MissingFloatingCanvas.to_string(),
            "floating target lacks canvas position"
        );
        assert_eq!(
            DropPlanError::InvalidCoordinate.to_string(),
            "invalid drop coordinate"
        );
        assert_eq!(
            DropPlanError::PanelTarget.to_string(),
            "panel is not a drop target"
        );
    }

    #[test]
    fn target_and_postcondition_descriptions_carry_ids_only() {
        assert_eq!(DropTarget::Panel.describe(), "panel");
        assert_eq!(
            DropTarget::Workspace { workspace_id: 2 }.describe(),
            "workspace 2"
        );
        assert_eq!(
            DropTarget::Canvas {
                workspace_id: 2,
                point: DropPoint { x: 0.25, y: 0.5 },
            }
            .describe(),
            "canvas 2 at (0.250, 0.500)"
        );
        assert_eq!(
            DropTarget::Window {
                workspace_id: 2,
                window_id: 13,
                fraction: 0.75,
                canvas_point: None,
            }
            .describe(),
            "window 13 in workspace 2 at 0.750"
        );
        assert_eq!(
            DropConfirmation::ExactProjection.describe(),
            "exact layout projection"
        );
        assert_eq!(
            DropConfirmation::JoinedWith {
                workspace_id: 2,
                target_window_id: 13,
            }
            .describe(),
            "joined with window 13 in workspace 2"
        );
    }

    #[test]
    fn emptied_source_workspace_confirms_after_niri_destroys_it() {
        // A drop that takes the last window off a workspace projects that
        // workspace as empty. Niri then removes it, so confirmation must
        // accept its absence instead of waiting for a snapshot that can
        // never arrive.
        let mut plan = plan_drop(
            &snapshot(),
            request(11, DropTarget::Workspace { workspace_id: 2 }),
        )
        .unwrap();
        assert_eq!(
            plan.projection,
            DropProjection::Exact(vec![
                ProjectedWorkspace {
                    workspace_id: 1,
                    columns: vec![vec![12], vec![13]],
                    floating: vec![31],
                },
                ProjectedWorkspace {
                    workspace_id: 2,
                    columns: vec![vec![21], vec![11]],
                    floating: Vec::new(),
                },
            ])
        );
        // The live layout right after Niri executed the move.
        let mut moved = snapshot();
        let output = moved.outputs.get_mut("left").unwrap();
        output.workspaces = vec![
            workspace(
                1,
                &[
                    (1, vec![window(12, 1, Some(1), false)]),
                    (2, vec![window(13, 1, Some(2), false)]),
                ],
                vec![window(31, 1, None, true)],
            ),
            workspace(
                2,
                &[
                    (1, vec![window(21, 2, Some(1), false)]),
                    (2, vec![window(11, 2, Some(2), false)]),
                ],
                Vec::new(),
            ),
        ];
        assert!(plan.postcondition_matches(&moved));

        // The source empties only when every other window leaves too; model
        // the whole source workspace vanishing and confirm against a live
        // snapshot without it.
        let mut vanished = moved;
        let output = vanished.outputs.get_mut("left").unwrap();
        output.workspaces.retain(|workspace| workspace.id != 1);
        plan.projection = DropProjection::Exact(vec![
            ProjectedWorkspace {
                workspace_id: 1,
                columns: Vec::new(),
                floating: Vec::new(),
            },
            ProjectedWorkspace {
                workspace_id: 2,
                columns: vec![vec![21], vec![11]],
                floating: Vec::new(),
            },
        ]);
        assert!(
            plan.postcondition_matches(&vanished),
            "a destroyed empty workspace must satisfy an empty projection"
        );

        // A missing workspace with expected content is still a mismatch.
        plan.projection = DropProjection::Exact(vec![ProjectedWorkspace {
            workspace_id: 1,
            columns: vec![vec![12]],
            floating: Vec::new(),
        }]);
        assert!(!plan.postcondition_matches(&vanished));
    }

    #[test]
    fn helper_indices_keep_niri_one_based_semantics() {
        assert_eq!(tiled_drop_position(1, 0.1, false), (1, None));
        assert_eq!(tiled_drop_position(4, 0.9, false), (5, None));
        assert_eq!(adjust_same_workspace_index(3, Some(1), true), 2);
        assert_eq!(adjust_same_workspace_index(3, Some(4), true), 3);
    }
}
