mod anonymous_query_handler;
mod bitcoin;
mod canister_manager;
mod canister_settings;
pub mod execution;
mod execution_environment;
mod execution_environment_metrics;
mod history;
mod hypervisor;
mod ic00_permissions;
mod ingress_filter;
mod metrics;
mod query_handler;
mod scheduler;
mod types;
pub mod util;

use crate::anonymous_query_handler::AnonymousQueryHandler;
pub use execution_environment::{
    as_num_instructions, as_round_instructions, execute_canister, CompilationCostHandling,
    ExecuteMessageResult, ExecutionEnvironment, ExecutionResponse, RoundInstructions, RoundLimits,
};
pub use history::{IngressHistoryReaderImpl, IngressHistoryWriterImpl};
pub use hypervisor::{Hypervisor, HypervisorMetrics};
use ic_base_types::PrincipalId;
use ic_config::{execution_environment::Config, subnet_config::SchedulerConfig};
use ic_cycles_account_manager::CyclesAccountManager;
use ic_interfaces::execution_environment::AnonymousQueryService;
use ic_interfaces::execution_environment::{
    IngressFilterService, IngressHistoryReader, IngressHistoryWriter, QueryExecutionService,
    QueryHandler, Scheduler,
};
use ic_interfaces_state_manager::StateReader;
use ic_logger::ReplicaLogger;
use ic_metrics::MetricsRegistry;
use ic_registry_subnet_type::SubnetType;
use ic_replicated_state::page_map::PageAllocatorFileDescriptor;
use ic_replicated_state::{CallOrigin, NetworkTopology, ReplicatedState};
use ic_types::{messages::CallContextId, SubnetId};
use ingress_filter::IngressFilter;
pub use query_handler::InternalHttpQueryHandler;
use query_handler::{HttpQueryHandler, QueryScheduler, QuerySchedulerFlag};
pub use scheduler::RoundSchedule;
use scheduler::SchedulerImpl;
use std::sync::Arc;

/// When executing a wasm method of query type, this enum indicates if we are
/// running in an replicated or non-replicated context. This information is
/// needed for various purposes and in particular to support the CoW memory
/// work.
#[doc(hidden)]
pub enum QueryExecutionType {
    /// The execution is happening in a replicated context (i.e. consensus was
    /// used to agree that this method should be executed). This should
    /// generally indicate that the message being handled in an Ingress or an
    /// inter-canister Request.
    Replicated,

    /// The execution is happening in a non-replicated context (i.e. consensus
    /// was not used to agree that this method should be executed). This should
    /// generally indicate that the message being handled is a Query message.
    NonReplicated {
        call_context_id: CallContextId,
        network_topology: Arc<NetworkTopology>,
        query_kind: NonReplicatedQueryKind,
    },
}

/// This enum indicates whether execution of a non-replicated query
/// should keep track of the state or not.
#[doc(hidden)]
#[derive(Clone, PartialEq, Eq)]
pub enum NonReplicatedQueryKind {
    Stateful { call_origin: CallOrigin },
    Pure { caller: PrincipalId },
}

// This struct holds public facing components that are created by Execution.
pub struct ExecutionServices {
    pub ingress_filter: IngressFilterService,
    pub ingress_history_writer: Arc<dyn IngressHistoryWriter<State = ReplicatedState>>,
    pub ingress_history_reader: Box<dyn IngressHistoryReader>,
    pub sync_query_handler: Arc<dyn QueryHandler<State = ReplicatedState>>,
    pub async_query_handler: QueryExecutionService,
    pub anonymous_query_handler: AnonymousQueryService,
    pub scheduler: Box<dyn Scheduler<State = ReplicatedState>>,
}

impl ExecutionServices {
    /// Constructs the public facing components that the
    /// `ExecutionEnvironment` crate exports.
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    pub fn setup_execution(
        logger: ReplicaLogger,
        metrics_registry: &MetricsRegistry,
        own_subnet_id: SubnetId,
        own_subnet_type: SubnetType,
        scheduler_config: SchedulerConfig,
        config: Config,
        cycles_account_manager: Arc<CyclesAccountManager>,
        state_reader: Arc<dyn StateReader<State = ReplicatedState>>,
        fd_factory: Arc<dyn PageAllocatorFileDescriptor>,
    ) -> ExecutionServices {
        let hypervisor = Arc::new(Hypervisor::new(
            config.clone(),
            metrics_registry,
            own_subnet_id,
            own_subnet_type,
            logger.clone(),
            Arc::clone(&cycles_account_manager),
            scheduler_config.dirty_page_overhead,
            Arc::clone(&fd_factory),
        ));

        let ingress_history_writer = Arc::new(IngressHistoryWriterImpl::new(
            config.clone(),
            logger.clone(),
            metrics_registry,
        ));
        let ingress_history_reader =
            Box::new(IngressHistoryReaderImpl::new(Arc::clone(&state_reader)));

        let exec_env = Arc::new(ExecutionEnvironment::new(
            logger.clone(),
            Arc::clone(&hypervisor),
            Arc::clone(&ingress_history_writer) as Arc<_>,
            metrics_registry,
            own_subnet_id,
            own_subnet_type,
            SchedulerImpl::compute_capacity_percent(scheduler_config.scheduler_cores),
            config.clone(),
            Arc::clone(&cycles_account_manager),
        ));
        let sync_query_handler = Arc::new(InternalHttpQueryHandler::new(
            logger.clone(),
            hypervisor,
            own_subnet_type,
            config.clone(),
            metrics_registry,
            scheduler_config.max_instructions_per_message_without_dts,
            Arc::clone(&cycles_account_manager),
        ));

        // If this is not a system or verified subnet we can double the
        // number of query execution threads total because the file-backed
        // allocator is enabled on these subnets and thus we can fit more
        // concurrent queries in memory.
        // TODO: remove this once the file-backed allocator is enabled on all subnets.
        let query_execution_threads_total = if own_subnet_type == SubnetType::Application {
            config.query_execution_threads_total * 2
        } else {
            config.query_execution_threads_total
        };
        let query_scheduler = QueryScheduler::new(
            query_execution_threads_total,
            config.query_execution_threads_per_canister,
            config.query_scheduling_time_slice_per_canister,
            metrics_registry,
            QuerySchedulerFlag::UseNewSchedulingAlgorithm,
        );

        // Creating the async services require that a tokio runtime context is available.

        let async_query_handler = HttpQueryHandler::new_service(
            Arc::clone(&sync_query_handler) as Arc<_>,
            query_scheduler.clone(),
            Arc::clone(&state_reader),
        );
        let ingress_filter = IngressFilter::new_service(
            query_scheduler.clone(),
            Arc::clone(&state_reader),
            Arc::clone(&exec_env),
        );
        let anonymous_query_handler = AnonymousQueryHandler::new_service(
            query_scheduler,
            Arc::clone(&state_reader),
            Arc::clone(&exec_env),
            scheduler_config.max_instructions_per_message_without_dts,
        );

        let scheduler = Box::new(SchedulerImpl::new(
            scheduler_config,
            own_subnet_id,
            Arc::clone(&ingress_history_writer) as Arc<_>,
            Arc::clone(&exec_env) as Arc<_>,
            Arc::clone(&cycles_account_manager),
            metrics_registry,
            logger,
            config.rate_limiting_of_heap_delta,
            config.rate_limiting_of_instructions,
            config.deterministic_time_slicing,
        ));

        Self {
            ingress_filter,
            ingress_history_writer,
            ingress_history_reader,
            sync_query_handler,
            async_query_handler,
            anonymous_query_handler,
            scheduler,
        }
    }

    #[allow(clippy::type_complexity)]
    pub fn into_parts(
        self,
    ) -> (
        IngressFilterService,
        Arc<dyn IngressHistoryWriter<State = ReplicatedState>>,
        Box<dyn IngressHistoryReader>,
        Arc<dyn QueryHandler<State = ReplicatedState>>,
        QueryExecutionService,
        AnonymousQueryService,
        Box<dyn Scheduler<State = ReplicatedState>>,
    ) {
        (
            self.ingress_filter,
            self.ingress_history_writer,
            self.ingress_history_reader,
            self.sync_query_handler,
            self.async_query_handler,
            self.anonymous_query_handler,
            self.scheduler,
        )
    }
}
