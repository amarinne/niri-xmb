//! The drop transaction: submit, await the action result, then confirm it
//! against the event stream before answering the caller.
//!
//! A drop is answered by its transaction rather than by the control
//! socket, because "the action returned" and "Niri actually did it" are
//! different events and only the second is worth reporting as success.

use crate::drop_plan;
use crate::ipc;
use crate::model::Snapshot;
use crate::niri::{ActionStep, ActionTransactionResult, NiriRequest, RestorationStatus};
use std::time::{Duration, Instant};

/// Let events from a failed partial action sequence reach the canonical model.
const DROP_FAILURE_RECONCILE: Duration = Duration::from_millis(120);

#[derive(Default)]
pub(crate) struct DropTransactionState {
    next_request_id: u64,
    pending: Option<PendingDropTransaction>,
    last: Option<CompletedDropTransaction>,
}

struct PendingDropTransaction {
    request_id: u64,
    plan: drop_plan::DropPlan,
    submitted_at: Instant,
    action_result: Option<ActionTransactionResult>,
    action_result_at: Option<Instant>,
    reply: Option<crossbeam_channel::Sender<String>>,
}

struct CompletedDropTransaction {
    request_id: u64,
    base_generation: u64,
    final_generation: u64,
    outcome: DropOutcome,
    action_result: Option<ActionTransactionResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DropOutcome {
    Confirmed,
    ActionFailed(String),
    ConfirmationTimedOut,
    /// The Niri actions did not come back inside their share of the
    /// budget. Distinct from ConfirmationTimedOut, which means the actions
    /// returned but the event stream never showed the result.
    ActionTimedOut,
    EventStreamUnavailable,
}

impl DropTransactionState {
    pub(crate) fn submit(
        &mut self,
        plan: drop_plan::DropPlan,
        snapshot: &Snapshot,
        reply: Option<crossbeam_channel::Sender<String>>,
        niri_tx: &crossbeam_channel::Sender<NiriRequest>,
    ) -> std::result::Result<u64, &'static str> {
        if self.pending.is_some() {
            return Err("drop transaction already active");
        }
        if plan.base_generation != snapshot.generation {
            return Err("stale drop plan");
        }
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        let request_id = self.next_request_id;
        tracing::info!(
            request_id,
            source = plan.source_window_id,
            base_generation = plan.base_generation,
            postcondition = %plan.confirmation.describe(),
            "drop transaction submitted"
        );
        let command = plan.clone().into_command();
        niri_tx
            .send(NiriRequest {
                request_id: Some(request_id),
                command,
            })
            .map_err(|_| "Niri command worker unavailable")?;
        self.pending = Some(PendingDropTransaction {
            request_id,
            plan,
            submitted_at: Instant::now(),
            action_result: None,
            action_result_at: None,
            reply,
        });
        Ok(request_id)
    }

    pub(crate) fn record_action_result(&mut self, result: ActionTransactionResult, now: Instant) {
        let Some(pending) = self.pending.as_mut() else {
            tracing::warn!(
                request_id = result.request_id,
                "orphan Niri transaction result"
            );
            return;
        };
        if pending.request_id != result.request_id {
            tracing::warn!(
                expected = pending.request_id,
                received = result.request_id,
                "out-of-order Niri transaction result"
            );
            return;
        }
        let steps = result
            .completed_steps
            .iter()
            .map(|step| action_step_name(*step))
            .collect::<Vec<_>>()
            .join(",");
        if let Some(failure) = &result.failure {
            tracing::warn!(
                request_id = result.request_id,
                completed_steps = %steps,
                failed_step = action_step_name(failure.step),
                error = %failure.error,
                "drop action step failed"
            );
        } else {
            tracing::debug!(
                request_id = result.request_id,
                completed_steps = %steps,
                "drop action steps completed"
            );
        }
        pending.action_result = Some(result);
        pending.action_result_at = Some(now);
    }

    pub(crate) fn reconcile(
        &mut self,
        snapshot: &Snapshot,
        event_stream_connected: bool,
        now: Instant,
    ) {
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        let submitted_at = pending.submitted_at;
        let has_result = pending.action_result.is_some();
        if !has_result {
            // No result yet: the actions are still running. They get the part
            // of the budget confirmation does not, and then they are over.
            if now.duration_since(submitted_at) >= ipc::DROP_ACTION_BUDGET {
                self.finish(snapshot.generation, DropOutcome::ActionTimedOut);
            }
            return;
        }
        let pending = self.pending.as_ref().unwrap();
        let result = pending.action_result.as_ref().unwrap();
        let result_at = pending.action_result_at.unwrap_or(pending.submitted_at);
        let outcome = if let Some(failure) = &result.failure {
            (now.duration_since(result_at) >= DROP_FAILURE_RECONCILE).then(|| {
                DropOutcome::ActionFailed(format!(
                    "{}: {}",
                    action_step_name(failure.step),
                    failure.error
                ))
            })
        } else if pending.plan.postcondition_matches(snapshot) {
            Some(DropOutcome::Confirmed)
        } else if !event_stream_connected {
            Some(DropOutcome::EventStreamUnavailable)
        } else if now.duration_since(result_at) >= ipc::DROP_CONFIRM_TIMEOUT {
            Some(DropOutcome::ConfirmationTimedOut)
        } else {
            None
        };
        if let Some(outcome) = outcome {
            self.finish(snapshot.generation, outcome);
        }
    }

    fn finish(&mut self, final_generation: u64, outcome: DropOutcome) {
        let Some(mut pending) = self.pending.take() else {
            return;
        };
        tracing::info!(
            request_id = pending.request_id,
            base_generation = pending.plan.base_generation,
            final_generation,
            outcome = drop_outcome_name(&outcome),
            "drop transaction finished"
        );
        let response = match &outcome {
            DropOutcome::Confirmed => "ok\n".to_string(),
            DropOutcome::ActionFailed(error) => format!("error: drop action failed: {error}\n"),
            DropOutcome::ConfirmationTimedOut => "error: drop confirmation timeout\n".to_string(),
            DropOutcome::ActionTimedOut => "error: drop action timeout\n".to_string(),
            DropOutcome::EventStreamUnavailable => {
                "error: Niri event stream unavailable\n".to_string()
            }
        };
        if let Some(reply) = pending.reply.take() {
            let _ = reply.send(response);
        }
        self.last = Some(CompletedDropTransaction {
            request_id: pending.request_id,
            base_generation: pending.plan.base_generation,
            final_generation,
            outcome,
            action_result: pending.action_result,
        });
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let pending = self.pending.as_ref()?;
        let Some(result_at) = pending.action_result_at else {
            // Actions still running: wake when their budget expires.
            return Some(pending.submitted_at + ipc::DROP_ACTION_BUDGET);
        };
        Some(
            if pending
                .action_result
                .as_ref()
                .is_some_and(|result| result.failure.is_some())
            {
                result_at + DROP_FAILURE_RECONCILE
            } else {
                result_at + ipc::DROP_CONFIRM_TIMEOUT
            },
        )
    }

    pub(crate) fn status_json(&self) -> serde_json::Value {
        if let Some(pending) = &self.pending {
            return serde_json::json!({
                "state": if pending.action_result.is_some() { "confirming" } else { "executing" },
                "request_id": pending.request_id,
                "base_generation": pending.plan.base_generation,
                "completed_steps": pending.action_result.as_ref().map(|result| {
                    result.completed_steps.iter().map(|step| action_step_name(*step)).collect::<Vec<_>>()
                }).unwrap_or_default(),
                "failed_step": pending.action_result.as_ref().and_then(|result| result.failure.as_ref())
                    .map(|failure| action_step_name(failure.step)),
            });
        }
        if let Some(last) = &self.last {
            return serde_json::json!({
                "state": drop_outcome_name(&last.outcome),
                "request_id": last.request_id,
                "base_generation": last.base_generation,
                "final_generation": last.final_generation,
                "completed_steps": last.action_result.as_ref().map(|result| {
                    result.completed_steps.iter().map(|step| action_step_name(*step)).collect::<Vec<_>>()
                }).unwrap_or_default(),
                "failed_step": last.action_result.as_ref().and_then(|result| result.failure.as_ref())
                    .map(|failure| action_step_name(failure.step)),
                "focus_restoration": last.action_result.as_ref().map(|result| restoration_name(&result.focus_restoration)),
                "cursor_restoration": last.action_result.as_ref().map(|result| restoration_name(&result.cursor_restoration)),
            });
        }
        serde_json::json!({ "state": "idle" })
    }
}

fn action_step_name(step: ActionStep) -> &'static str {
    match step {
        ActionStep::FocusWindow => "focus-window",
        ActionStep::FocusWorkspace => "focus-workspace",
        ActionStep::DetachJoinedSource => "detach-joined-source",
        ActionStep::MoveToTiling => "move-to-tiling",
        ActionStep::MoveToFloating => "move-to-floating",
        ActionStep::MoveToWorkspace => "move-to-workspace",
        ActionStep::FocusMovedWindow => "focus-moved-window",
        ActionStep::MoveColumn => "move-column",
        ActionStep::JoinLeft => "join-left",
        ActionStep::MoveFloatingWindow => "move-floating-window",
        ActionStep::RestoreFocus => "restore-focus",
    }
}

fn drop_outcome_name(outcome: &DropOutcome) -> &'static str {
    match outcome {
        DropOutcome::Confirmed => "confirmed",
        DropOutcome::ActionFailed(_) => "action_failed",
        DropOutcome::ConfirmationTimedOut => "confirmation_timeout",
        DropOutcome::ActionTimedOut => "action_timeout",
        DropOutcome::EventStreamUnavailable => "event_stream_unavailable",
    }
}

fn restoration_name(status: &RestorationStatus) -> &'static str {
    match status {
        RestorationStatus::NotRequested => "not_requested",
        RestorationStatus::Completed => "completed",
        RestorationStatus::Failed(_) => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{OutputModel, WindowModel, WorkspaceModel};
    use std::collections::BTreeMap;

    fn protocol_snapshot() -> Snapshot {
        let window = WindowModel {
            id: 42,
            workspace_id: 7,
            app_id: "org.example.App".into(),
            title: "Example".into(),
            focused: true,
            urgent: false,
            floating: false,
            column: Some(2),
            tile_width: 800.0,
            tile_height: 600.0,
            x: Some(10.0),
            y: Some(20.0),
        };
        Snapshot {
            outputs: BTreeMap::from([(
                "eDP-1".into(),
                OutputModel {
                    name: "eDP-1".into(),
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    workspaces: vec![WorkspaceModel {
                        id: 7,
                        index: 1,
                        name: Some("main".into()),
                        active: true,
                        focused: true,
                        urgent: false,
                        active_window: Some(42),
                        columns: BTreeMap::from([(2, BTreeMap::from([(3, window)]))]),
                        floating: Vec::new(),
                    }],
                },
            )]),
            focused_window: Some(42),
            overview_open: false,
            generation: 9,
            content_revision: 0,
        }
    }

    fn transaction_plan(generation: u64) -> drop_plan::DropPlan {
        drop_plan::DropPlan {
            base_generation: generation,
            source_window_id: 42,
            source_workspace_id: 7,
            source_joined: false,
            destination: drop_plan::DropDestination::Tiled {
                workspace_id: 7,
                column: 1,
                join: None,
            },
            // A projection no live snapshot can ever satisfy: the workspace
            // does not exist and the expectation is non-empty. (An empty
            // expectation would be satisfied by absence, since Niri destroys
            // emptied workspaces.)
            projection: drop_plan::DropProjection::Exact(vec![drop_plan::ProjectedWorkspace {
                workspace_id: u64::MAX,
                columns: vec![vec![42]],
                floating: Vec::new(),
            }]),
            confirmation: drop_plan::DropConfirmation::ExactProjection,
            restore_focus: None,
            restore_cursor: None,
        }
    }

    fn confirmed_plan(generation: u64) -> drop_plan::DropPlan {
        let mut plan = transaction_plan(generation);
        // An empty exact projection is vacuously satisfied, so any snapshot
        // after a successful action confirms the transaction.
        plan.projection = drop_plan::DropProjection::Exact(Vec::new());
        plan
    }

    fn failed_result(request_id: u64) -> crate::niri::ActionTransactionResult {
        crate::niri::ActionTransactionResult {
            request_id,
            completed_steps: vec![ActionStep::MoveToWorkspace],
            failure: Some(crate::niri::ActionStepFailure {
                step: ActionStep::MoveColumn,
                error: "column move rejected".into(),
            }),
            focus_restoration: crate::niri::RestorationStatus::Completed,
            cursor_restoration: crate::niri::RestorationStatus::NotRequested,
        }
    }

    #[test]
    fn drop_transaction_confirms_after_successful_action() {
        let snapshot = protocol_snapshot();
        let (niri_tx, niri_rx) = crossbeam_channel::unbounded();
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let mut state = DropTransactionState::default();

        let request_id = state
            .submit(
                confirmed_plan(snapshot.generation),
                &snapshot,
                Some(reply_tx),
                &niri_tx,
            )
            .unwrap();
        assert!(niri_rx.try_recv().is_ok());
        assert!(reply_rx.try_recv().is_err(), "reply must stay deferred");

        let submitted_at = Instant::now();
        // Before the action worker reports, nothing may confirm or reply.
        state.reconcile(&snapshot, true, submitted_at);
        assert!(reply_rx.try_recv().is_err());

        state.record_action_result(
            crate::niri::ActionTransactionResult {
                request_id,
                completed_steps: vec![ActionStep::FocusWindow, ActionStep::MoveColumn],
                failure: None,
                focus_restoration: crate::niri::RestorationStatus::Completed,
                cursor_restoration: crate::niri::RestorationStatus::NotRequested,
            },
            submitted_at,
        );

        state.reconcile(&snapshot, true, submitted_at + Duration::from_millis(1));
        assert_eq!(reply_rx.try_recv().unwrap(), "ok\n");
        assert!(state.next_deadline().is_none());
    }

    #[test]
    fn drop_transaction_reports_failed_step_and_keeps_reply() {
        let snapshot = protocol_snapshot();
        let (niri_tx, _niri_rx) = crossbeam_channel::unbounded();
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let mut state = DropTransactionState::default();

        let request_id = state
            .submit(
                transaction_plan(snapshot.generation),
                &snapshot,
                Some(reply_tx),
                &niri_tx,
            )
            .unwrap();

        let result_at = Instant::now();
        state.record_action_result(failed_result(request_id), result_at);
        // The failure holds briefly so an immediately following event burst
        // cannot race the structured result.
        state.reconcile(&snapshot, true, result_at + Duration::from_millis(1));
        assert!(reply_rx.try_recv().is_err());

        state.reconcile(
            &snapshot,
            true,
            result_at + DROP_FAILURE_RECONCILE + Duration::from_millis(1),
        );
        assert_eq!(
            reply_rx.try_recv().unwrap(),
            "error: drop action failed: move-column: column move rejected\n"
        );
    }

    #[test]
    fn drop_transaction_times_out_without_confirmation() {
        let snapshot = protocol_snapshot();
        let (niri_tx, _niri_rx) = crossbeam_channel::unbounded();
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let mut state = DropTransactionState::default();

        let request_id = state
            .submit(
                transaction_plan(snapshot.generation),
                &snapshot,
                Some(reply_tx),
                &niri_tx,
            )
            .unwrap();

        let result_at = Instant::now();
        state.record_action_result(
            crate::niri::ActionTransactionResult {
                request_id,
                completed_steps: vec![ActionStep::MoveColumn],
                failure: None,
                focus_restoration: crate::niri::RestorationStatus::Completed,
                cursor_restoration: crate::niri::RestorationStatus::NotRequested,
            },
            result_at,
        );
        state.reconcile(
            &snapshot,
            true,
            result_at + ipc::DROP_CONFIRM_TIMEOUT + Duration::from_millis(1),
        );
        assert_eq!(
            reply_rx.try_recv().unwrap(),
            "error: drop confirmation timeout\n"
        );
    }

    /// Niri actions used to have to finish inside about 800 ms or the control
    /// socket gave up and reported a failure for a drop that was still being
    /// confirmed. A slow action sequence must still be able to reply ok.
    #[test]
    fn a_slow_action_sequence_still_confirms() {
        let snapshot = protocol_snapshot();
        let (niri_tx, _niri_rx) = crossbeam_channel::unbounded();
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let mut state = DropTransactionState::default();

        let request_id = state
            .submit(
                confirmed_plan(snapshot.generation),
                &snapshot,
                Some(reply_tx),
                &niri_tx,
            )
            .unwrap();

        // The actions take longer than the old 2 s reply bound allowed.
        let submitted_at = Instant::now();
        let result_at = submitted_at + Duration::from_millis(2500);
        state.record_action_result(
            crate::niri::ActionTransactionResult {
                request_id,
                completed_steps: vec![ActionStep::MoveColumn],
                failure: None,
                focus_restoration: crate::niri::RestorationStatus::Completed,
                cursor_restoration: crate::niri::RestorationStatus::NotRequested,
            },
            result_at,
        );
        state.reconcile(&snapshot, true, result_at + Duration::from_millis(1));

        assert_eq!(reply_rx.try_recv().unwrap(), "ok\n");
    }

    #[test]
    fn drop_transaction_rejects_overlap_and_stale_plans() {
        let snapshot = protocol_snapshot();
        let (niri_tx, _niri_rx) = crossbeam_channel::unbounded();
        let mut state = DropTransactionState::default();

        state
            .submit(
                transaction_plan(snapshot.generation),
                &snapshot,
                None,
                &niri_tx,
            )
            .unwrap();
        assert_eq!(
            state.submit(
                transaction_plan(snapshot.generation),
                &snapshot,
                None,
                &niri_tx
            ),
            Err("drop transaction already active")
        );

        let mut stale = DropTransactionState::default();
        assert_eq!(
            stale.submit(
                transaction_plan(snapshot.generation - 1),
                &snapshot,
                None,
                &niri_tx
            ),
            Err("stale drop plan")
        );
    }

    #[test]
    fn drop_transaction_survives_event_stream_loss() {
        let snapshot = protocol_snapshot();
        let (niri_tx, _niri_rx) = crossbeam_channel::unbounded();
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let mut state = DropTransactionState::default();

        let request_id = state
            .submit(
                transaction_plan(snapshot.generation),
                &snapshot,
                Some(reply_tx),
                &niri_tx,
            )
            .unwrap();

        let result_at = Instant::now();
        state.record_action_result(
            crate::niri::ActionTransactionResult {
                request_id,
                completed_steps: vec![ActionStep::MoveColumn],
                failure: None,
                focus_restoration: crate::niri::RestorationStatus::Completed,
                cursor_restoration: crate::niri::RestorationStatus::NotRequested,
            },
            result_at,
        );
        // Without events no postcondition can ever be observed; report that
        // instead of waiting for a timeout that proves nothing.
        state.reconcile(&snapshot, false, result_at + Duration::from_millis(1));
        assert_eq!(
            reply_rx.try_recv().unwrap(),
            "error: Niri event stream unavailable\n"
        );
    }

    /// Actions that never come back used to leave the transaction pending
    /// forever -- reconcile returned early with no result, and next_deadline
    /// returned None, so the daemon never finished it at all.
    #[test]
    fn actions_that_never_return_time_out() {
        let snapshot = protocol_snapshot();
        let (niri_tx, _niri_rx) = crossbeam_channel::unbounded();
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        let mut state = DropTransactionState::default();

        state
            .submit(
                transaction_plan(snapshot.generation),
                &snapshot,
                Some(reply_tx),
                &niri_tx,
            )
            .unwrap();

        let submitted_at = Instant::now();
        // Still inside the action budget: nothing is decided yet.
        state.reconcile(&snapshot, true, submitted_at + Duration::from_millis(1));
        assert!(reply_rx.try_recv().is_err());
        // And the loop knows when to come back.
        assert!(state.next_deadline().is_some());

        state.reconcile(
            &snapshot,
            true,
            submitted_at + ipc::DROP_ACTION_BUDGET + Duration::from_millis(1),
        );
        assert_eq!(reply_rx.try_recv().unwrap(), "error: drop action timeout\n");
    }
}
