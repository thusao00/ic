// This module defines how update messages and canister tasks are executed.
// See https://smartcontracts.org/docs/interface-spec/index.html#rule-message-execution

use crate::execution::common::{
    action_to_response, apply_canister_state_changes, finish_call_with_error,
    ingress_status_with_processing_state, update_round_limits, validate_message,
};
use crate::execution_environment::{
    ExecuteMessageResult, PausedExecution, RoundContext, RoundLimits,
};
use ic_base_types::CanisterId;
use ic_embedders::wasm_executor::{CanisterStateChanges, PausedWasmExecution, WasmExecutionResult};
use ic_error_types::{ErrorCode, UserError};
use ic_interfaces::execution_environment::{
    CanisterOutOfCyclesError, HypervisorError, WasmExecutionOutput,
};
use ic_interfaces::messages::{CanisterCall, CanisterMessageOrTask, CanisterTask};
use ic_interfaces::messages::{CanisterCallOrTask, CanisterMessage};
use ic_logger::{info, ReplicaLogger};
use ic_replicated_state::{CallOrigin, CanisterState};
use ic_types::messages::CallContextId;
use ic_types::{CanisterTimer, Cycles, NumBytes, NumInstructions, Time};
use ic_wasm_types::WasmEngineError::FailedToApplySystemChanges;

use ic_system_api::{ApiType, ExecutionParameters};
use ic_types::methods::{FuncRef, SystemMethod, WasmMethod};

#[cfg(test)]
mod tests;

// Execute an inter-canister call message or a canister task.
#[allow(clippy::too_many_arguments)]
pub fn execute_update(
    clean_canister: CanisterState,
    call_or_task: CanisterCallOrTask,
    method: WasmMethod,
    prepaid_execution_cycles: Option<Cycles>,
    execution_parameters: ExecutionParameters,
    time: Time,
    round: RoundContext,
    round_limits: &mut RoundLimits,
    subnet_size: usize,
) -> ExecuteMessageResult {
    let (clean_canister, prepaid_execution_cycles, resuming_aborted) =
        match prepaid_execution_cycles {
            Some(prepaid_execution_cycles) => (clean_canister, prepaid_execution_cycles, true),
            None => {
                let mut canister = clean_canister;
                let memory_usage = canister.memory_usage();
                let prepaid_execution_cycles =
                    match round.cycles_account_manager.prepay_execution_cycles(
                        &mut canister.system_state,
                        memory_usage,
                        execution_parameters.compute_allocation,
                        execution_parameters.instruction_limits.message(),
                        subnet_size,
                    ) {
                        Ok(cycles) => cycles,
                        Err(err) => {
                            return finish_call_with_error(
                                UserError::new(ErrorCode::CanisterOutOfCycles, err),
                                canister,
                                call_or_task,
                                NumInstructions::from(0),
                                round.time,
                                execution_parameters.subnet_type,
                                round.log,
                            );
                        }
                    };
                (canister, prepaid_execution_cycles, false)
            }
        };

    let freezing_threshold = round.cycles_account_manager.freeze_threshold_cycles(
        clean_canister.system_state.freeze_threshold,
        clean_canister.system_state.memory_allocation,
        clean_canister.memory_usage(),
        clean_canister.compute_allocation(),
        subnet_size,
    );

    let original = OriginalContext {
        call_origin: CallOrigin::from(&call_or_task),
        method,
        call_or_task,
        prepaid_execution_cycles,
        execution_parameters,
        subnet_size,
        time,
        freezing_threshold,
        canister_id: clean_canister.canister_id(),
    };

    let helper = match UpdateHelper::new(&clean_canister, &original) {
        Ok(helper) => helper,
        Err(err) => {
            return finish_err(
                clean_canister,
                original.execution_parameters.instruction_limits.message(),
                err,
                original,
                round,
            )
        }
    };

    let api_type = match &original.call_or_task {
        CanisterCallOrTask::Call(msg) => ApiType::update(
            time,
            msg.method_payload().to_vec(),
            msg.cycles(),
            *msg.sender(),
            helper.call_context_id(),
        ),
        CanisterCallOrTask::Task(CanisterTask::Heartbeat) => ApiType::system_task(
            SystemMethod::CanisterHeartbeat,
            time,
            helper.call_context_id(),
        ),
        CanisterCallOrTask::Task(CanisterTask::GlobalTimer) => ApiType::system_task(
            SystemMethod::CanisterGlobalTimer,
            time,
            helper.call_context_id(),
        ),
    };

    let memory_usage = helper.canister().memory_usage();
    let result = round.hypervisor.execute_dts(
        api_type,
        helper.canister().execution_state.as_ref().unwrap(),
        &helper.canister().system_state,
        memory_usage,
        original.execution_parameters.clone(),
        FuncRef::Method(original.method.clone()),
        round_limits,
        round.network_topology,
    );
    match result {
        WasmExecutionResult::Paused(slice, paused_wasm_execution) => {
            info!(
                round.log,
                "[DTS] Pausing {:?} execution of canister {} after {} instructions.",
                original.method,
                clean_canister.canister_id(),
                slice.executed_instructions,
            );
            update_round_limits(round_limits, &slice);

            let ingress_status = match (resuming_aborted, &original.call_or_task) {
                (true, _) => {
                    // Resuming an aborted execution doesn't change the ingress
                    // status.
                    None
                }
                (false, CanisterCallOrTask::Task(_)) => {
                    // Canister tasks do not have ingress status.
                    None
                }
                (false, CanisterCallOrTask::Call(call)) => {
                    ingress_status_with_processing_state(call, original.time)
                }
            };
            let paused_execution = Box::new(PausedCallExecution {
                paused_wasm_execution,
                paused_helper: helper.pause(),
                original,
            });
            ExecuteMessageResult::Paused {
                canister: clean_canister,
                paused_execution,
                ingress_status,
            }
        }
        WasmExecutionResult::Finished(slice, output, state_changes) => {
            update_round_limits(round_limits, &slice);
            helper.finish(
                output,
                clean_canister,
                state_changes,
                original,
                round,
                round_limits,
            )
        }
    }
}

/// Finishes an update call execution early due to an error. The only state
/// change that is applied to the clean canister state is refunding the prepaid
/// execution cycles.
fn finish_err(
    clean_canister: CanisterState,
    instructions_left: NumInstructions,
    err: UserError,
    original: OriginalContext,
    round: RoundContext,
) -> ExecuteMessageResult {
    let mut canister = clean_canister;

    canister
        .system_state
        .apply_ingress_induction_cycles_debit(canister.canister_id(), round.log);

    let instruction_limit = original.execution_parameters.instruction_limits.message();
    round.cycles_account_manager.refund_unused_execution_cycles(
        &mut canister.system_state,
        instructions_left,
        instruction_limit,
        original.prepaid_execution_cycles,
        round.execution_refund_error_counter,
        original.subnet_size,
        round.log,
    );
    let instructions_used = instruction_limit - instructions_left;
    finish_call_with_error(
        err,
        canister,
        original.call_or_task,
        instructions_used,
        round.time,
        original.execution_parameters.subnet_type,
        round.log,
    )
}

/// Context variables that remain the same throughout the entire deterministic
/// time slicing execution of an update call execution.
#[derive(Debug)]
struct OriginalContext {
    call_origin: CallOrigin,
    call_or_task: CanisterCallOrTask,
    prepaid_execution_cycles: Cycles,
    method: WasmMethod,
    execution_parameters: ExecutionParameters,
    subnet_size: usize,
    time: Time,
    freezing_threshold: Cycles,
    canister_id: CanisterId,
}

/// Contains fields of `UpdateHelper` that are necessary for resuming an update
/// call execution.
#[derive(Debug)]
struct PausedUpdateHelper {
    call_context_id: CallContextId,
    initial_cycles_balance: Cycles,
}

/// A helper that implements and keeps track of update call steps.
/// It is used to safely pause and resume an update call execution.
struct UpdateHelper {
    canister: CanisterState,
    call_context_id: CallContextId,
    initial_cycles_balance: Cycles,
}

impl UpdateHelper {
    /// Applies the initial state changes and performs the initial validation.
    fn new(clean_canister: &CanisterState, original: &OriginalContext) -> Result<Self, UserError> {
        let mut canister = clean_canister.clone();

        validate_message(&canister, &original.method)?;

        let call_context_id = canister
            .system_state
            .call_context_manager_mut()
            .unwrap()
            .new_call_context(
                original.call_origin.clone(),
                original.call_or_task.cycles(),
                original.time,
            );

        let initial_cycles_balance = canister.system_state.balance();

        match original.call_or_task {
            CanisterCallOrTask::Call(_) | CanisterCallOrTask::Task(CanisterTask::Heartbeat) => {}
            CanisterCallOrTask::Task(CanisterTask::GlobalTimer) => {
                // The global timer is one-off.
                canister.system_state.global_timer = CanisterTimer::Inactive;
            }
        }

        Ok(Self {
            canister,
            call_context_id,
            initial_cycles_balance,
        })
    }

    /// Returns a struct with all the necessary information to replay the
    /// performed update call steps in subsequent rounds.
    fn pause(self) -> PausedUpdateHelper {
        PausedUpdateHelper {
            call_context_id: self.call_context_id,
            initial_cycles_balance: self.initial_cycles_balance,
        }
    }

    /// Replays the previous update call steps on the given clean canister.
    /// Returns an error if any step fails. Otherwise, it returns an instance of
    /// the helper that can be used to continue the update call execution.
    fn resume(
        clean_canister: &CanisterState,
        original: &OriginalContext,
        paused: PausedUpdateHelper,
    ) -> Result<Self, UserError> {
        let helper = Self::new(clean_canister, original)?;
        if helper.initial_cycles_balance != paused.initial_cycles_balance {
            let msg = "Mismatch in cycles balance when resuming an update call".to_string();
            let err = HypervisorError::WasmEngineError(FailedToApplySystemChanges(msg));
            return Err(err.into_user_error(&clean_canister.canister_id()));
        }
        if helper.call_context_id != paused.call_context_id {
            let msg = "Mismatch in call context id when resuming an update call".to_string();
            let err = HypervisorError::WasmEngineError(FailedToApplySystemChanges(msg));
            return Err(err.into_user_error(&clean_canister.canister_id()));
        }
        Ok(helper)
    }

    /// Finishes an update call execution that could have run multiple rounds
    /// due to determnistic time slicing.
    fn finish(
        mut self,
        mut output: WasmExecutionOutput,
        clean_canister: CanisterState,
        canister_state_changes: Option<CanisterStateChanges>,
        original: OriginalContext,
        round: RoundContext,
        round_limits: &mut RoundLimits,
    ) -> ExecuteMessageResult {
        self.canister
            .system_state
            .apply_ingress_induction_cycles_debit(self.canister.canister_id(), round.log);

        // Check that the cycles balance does not go below the freezing
        // threshold after applying the Wasm execution state changes.
        if let Some(state_changes) = &canister_state_changes {
            let old_balance = self.canister.system_state.balance();
            let requested = state_changes.system_state_changes.removed_cycles();
            if old_balance < requested + original.freezing_threshold {
                let err = CanisterOutOfCyclesError {
                    canister_id: self.canister.canister_id(),
                    available: old_balance,
                    requested,
                    threshold: original.freezing_threshold,
                };
                let err = UserError::new(ErrorCode::CanisterOutOfCycles, err);
                info!(
                    round.log,
                    "[DTS] Failed {:?} execution of canister {} due to concurrent cycle change: {:?}.",
                    original.method,
                    clean_canister.canister_id(),
                    err,
                );
                return finish_err(
                    clean_canister,
                    output.num_instructions_left,
                    err,
                    original,
                    round,
                );
            }
        }

        apply_canister_state_changes(
            canister_state_changes,
            self.canister.execution_state.as_mut().unwrap(),
            &mut self.canister.system_state,
            &mut output,
            round_limits,
            round.time,
            round.network_topology,
            round.hypervisor.subnet_id(),
            round.log,
        );
        let heap_delta = if output.wasm_result.is_ok() {
            NumBytes::from((output.instance_stats.dirty_pages * ic_sys::PAGE_SIZE) as u64)
        } else {
            NumBytes::from(0)
        };

        let action = self
            .canister
            .system_state
            .call_context_manager_mut()
            .unwrap()
            .on_canister_result(self.call_context_id, None, output.wasm_result);

        let response = action_to_response(
            &self.canister,
            action,
            original.call_origin,
            round.time,
            round.log,
        );
        round.cycles_account_manager.refund_unused_execution_cycles(
            &mut self.canister.system_state,
            output.num_instructions_left,
            original.execution_parameters.instruction_limits.message(),
            original.prepaid_execution_cycles,
            round.execution_refund_error_counter,
            original.subnet_size,
            round.log,
        );
        let instructions_used = NumInstructions::from(
            original
                .execution_parameters
                .instruction_limits
                .message()
                .get()
                .saturating_sub(output.num_instructions_left.get()),
        );
        ExecuteMessageResult::Finished {
            canister: self.canister,
            response,
            instructions_used,
            heap_delta,
        }
    }

    fn canister(&self) -> &CanisterState {
        &self.canister
    }

    fn call_context_id(&self) -> CallContextId {
        self.call_context_id
    }
}

#[derive(Debug)]
struct PausedCallExecution {
    paused_wasm_execution: Box<dyn PausedWasmExecution>,
    paused_helper: PausedUpdateHelper,
    original: OriginalContext,
}

impl PausedExecution for PausedCallExecution {
    fn resume(
        self: Box<Self>,
        clean_canister: CanisterState,
        round: RoundContext,
        round_limits: &mut RoundLimits,
        _subnet_size: usize,
    ) -> ExecuteMessageResult {
        info!(
            round.log,
            "[DTS] Resuming {:?} execution of canister {}.",
            self.original.method,
            clean_canister.canister_id(),
        );
        let helper = match UpdateHelper::resume(&clean_canister, &self.original, self.paused_helper)
        {
            Ok(helper) => helper,
            Err(err) => {
                info!(
                    round.log,
                    "[DTS] Failed to resume {:?} execution of canister {}: {:?}.",
                    self.original.method,
                    clean_canister.canister_id(),
                    err,
                );
                self.paused_wasm_execution.abort();
                return finish_err(
                    clean_canister,
                    self.original
                        .execution_parameters
                        .instruction_limits
                        .message(),
                    err,
                    self.original,
                    round,
                );
            }
        };

        let execution_state = helper.canister().execution_state.as_ref().unwrap();
        let result = self.paused_wasm_execution.resume(execution_state);
        match result {
            WasmExecutionResult::Paused(slice, paused_wasm_execution) => {
                info!(
                    round.log,
                    "[DTS] Pausing {:?} execution of canister {} after {} instructions.",
                    self.original.method,
                    clean_canister.canister_id(),
                    slice.executed_instructions,
                );
                update_round_limits(round_limits, &slice);
                let paused_execution = Box::new(PausedCallExecution {
                    paused_wasm_execution,
                    paused_helper: helper.pause(),
                    original: self.original,
                });
                ExecuteMessageResult::Paused {
                    canister: clean_canister,
                    paused_execution,
                    // Pausing a resumed execution doesn't change the ingress
                    // status.
                    ingress_status: None,
                }
            }
            WasmExecutionResult::Finished(slice, output, state_changes) => {
                info!(
                    round.log,
                    "[DTS] Finished {:?} execution of canister {} after {} / {} instructions.",
                    self.original.method,
                    clean_canister.canister_id(),
                    slice.executed_instructions,
                    self.original
                        .execution_parameters
                        .instruction_limits
                        .message()
                        - output.num_instructions_left,
                );
                update_round_limits(round_limits, &slice);
                helper.finish(
                    output,
                    clean_canister,
                    state_changes,
                    self.original,
                    round,
                    round_limits,
                )
            }
        }
    }

    fn abort(self: Box<Self>, log: &ReplicaLogger) -> (CanisterMessageOrTask, Cycles) {
        info!(
            log,
            "[DTS] Aborting {:?} execution of canister {}",
            self.original.method,
            self.original.canister_id,
        );
        self.paused_wasm_execution.abort();
        let message_or_task = match self.original.call_or_task {
            CanisterCallOrTask::Call(CanisterCall::Request(r)) => {
                CanisterMessageOrTask::Message(CanisterMessage::Request(r))
            }
            CanisterCallOrTask::Call(CanisterCall::Ingress(i)) => {
                CanisterMessageOrTask::Message(CanisterMessage::Ingress(i))
            }
            CanisterCallOrTask::Task(task) => CanisterMessageOrTask::Task(task),
        };
        (message_or_task, self.original.prepaid_execution_cycles)
    }
}
