/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::HashMap;
use std::io::BufWriter;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use async_trait::async_trait;
use buck2_build_api::actions::build_listener::BuildSignalSender;
use buck2_build_api::actions::build_listener::SetBuildSignals;
use buck2_build_api::actions::impls::run::knobs::HasRunActionKnobs;
use buck2_build_api::actions::impls::run::knobs::RunActionKnobs;
use buck2_build_api::context::SetBuildContextData;
use buck2_build_api::interpreter::context::configure_build_file_globals;
use buck2_build_api::interpreter::context::configure_extension_file_globals;
use buck2_build_api::interpreter::context::prelude_path;
use buck2_build_api::query::analysis::environment::ConfiguredGraphQueryEnvironment;
use buck2_build_api::spawner::BuckSpawner;
use buck2_common::dice::cells::HasCellResolver;
use buck2_common::executor_config::CacheUploadBehavior;
use buck2_common::executor_config::CommandExecutorConfig;
use buck2_common::executor_config::CommandExecutorKind;
use buck2_common::executor_config::LocalExecutorOptions;
use buck2_common::executor_config::PathSeparatorKind;
use buck2_common::io::IoProvider;
use buck2_common::legacy_configs::dice::HasLegacyConfigs;
use buck2_common::legacy_configs::LegacyBuckConfig;
use buck2_common::legacy_configs::LegacyBuckConfigs;
use buck2_common::result::SharedResult;
use buck2_common::result::ToSharedResultExt;
use buck2_core::async_once_cell::AsyncOnceCell;
use buck2_core::cells::CellResolver;
use buck2_core::facebook_only;
use buck2_core::fs::paths::AbsPath;
use buck2_core::fs::project::ProjectRelativePath;
use buck2_core::fs::project::ProjectRelativePathBuf;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::pattern::ParsedPattern;
use buck2_core::pattern::ProvidersPattern;
use buck2_events::dispatch::EventDispatcher;
use buck2_events::metadata;
use buck2_execute::execute::blocking::BlockingExecutor;
use buck2_execute::execute::blocking::SetBlockingExecutor;
use buck2_execute::execute::dice_data::set_fallback_executor_config;
use buck2_execute::execute::dice_data::SetCommandExecutor;
use buck2_execute::knobs::ExecutorGlobalKnobs;
use buck2_execute::materialize::materializer::Materializer;
use buck2_execute::materialize::materializer::SetMaterializer;
use buck2_execute::re::client::RemoteExecutionClient;
use buck2_execute::re::manager::ReConnectionHandle;
use buck2_execute::re::manager::ReConnectionManager;
use buck2_execute::re::manager::ReConnectionObserver;
use buck2_forkserver::client::ForkserverClient;
use buck2_interpreter::dice::interpreter_setup::setup_interpreter;
use buck2_interpreter::dice::starlark_profiler::StarlarkProfilerConfiguration;
use buck2_interpreter::extra::InterpreterConfiguror;
use buck2_interpreter_for_build::interpreter::configuror::BuildInterpreterConfiguror;
use buck2_server_ctx::concurrency::ConcurrencyHandler;
use buck2_server_ctx::concurrency::DiceDataProvider;
use buck2_server_ctx::concurrency::DiceUpdater;
use buck2_server_ctx::ctx::DiceAccessor;
use buck2_server_ctx::ctx::PrivateStruct;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use buck2_server_ctx::raw_output::RawOuputGuard;
use buck2_server_ctx::raw_output::RawOutputWriter;
use cli_proto::client_context::HostPlatformOverride;
use cli_proto::common_build_options::ExecutionStrategy;
use cli_proto::ClientContext;
use cli_proto::CommonBuildOptions;
use cli_proto::ConfigOverride;
use dice::data::DiceData;
use dice::DiceComputations;
use dice::DiceTransaction;
use dice::UserComputationData;
use gazebo::dupe::Dupe;
use gazebo::prelude::SliceExt;
use host_sharing::HostSharingBroker;
use host_sharing::HostSharingStrategy;
use starlark::environment::GlobalsBuilder;
use tracing::warn;

use crate::active_commands::ActiveCommandDropGuard;
use crate::configs::parse_legacy_cells;
use crate::daemon::common::get_executor_config_for_strategy;
use crate::daemon::common::parse_concurrency;
use crate::daemon::common::CommandExecutorFactory;
use crate::dice_tracker::BuckDiceTracker;
use crate::file_watcher::FileWatcher;
use crate::heartbeat_guard::HeartbeatGuard;
use crate::host_info;

#[derive(Debug, thiserror::Error)]
enum DaemonCommunicationError {
    #[error("Got invalid working directory `{0}`")]
    InvalidWorkingDirectory(String),
}

/// BaseCommandContext provides access to the global daemon state and information specific to a command (like the
/// EventDispatcher). Most commands use a ServerCommandContext which has more command/client-specific information.
pub struct BaseServerCommandContext {
    /// An fbinit token for using things that require fbinit. fbinit is initialized on daemon startup.
    pub _fb: fbinit::FacebookInit,
    /// Absolute path to the project root.
    pub project_root: ProjectRoot,
    /// A reference to the dice graph. Most interesting things are accessible from this (and new interesting things should be
    /// added there rather than as fields here). This has some per-request setup done already (like attaching a per-request
    /// event dispatcher).
    pub dice_manager: ConcurrencyHandler,
    /// A reference to the I/O provider.
    pub io: Arc<dyn IoProvider>,
    /// The RE connection, managed such that all build commands that are concurrently active uses
    /// the same connection.
    pub re_client_manager: Arc<ReConnectionManager>,
    /// Executor responsible for coordinating and rate limiting I/O.
    pub blocking_executor: Arc<dyn BlockingExecutor>,
    /// Object responsible for handling most materializations.
    pub materializer: Arc<dyn Materializer>,
    /// Forkserver connection, if any was started
    pub forkserver: Option<ForkserverClient>,
    /// The event dispatcher for this command context.
    pub events: EventDispatcher,
    /// Removes this command from the set of active commands when dropped.
    pub _drop_guard: ActiveCommandDropGuard,
    /// The file watcher that keeps buck2 up to date with disk changes.
    pub file_watcher: Arc<dyn FileWatcher>,
    /// Whether or not to hash all commands
    pub hash_all_commands: bool,
    /// Whether or not dep files should declare and match in the materializer
    pub declare_match_in_depfiles: bool,
    /// Whether or not we should have declare calls in the local executor.
    pub declare_in_local_executor: bool,
    /// Start time to track daemon uptime
    pub daemon_start_time: Instant,
}

impl BaseServerCommandContext {
    /// Provides a DiceComputations. This may be missing some data or injected keys that
    /// we normally expect. To get a full dice context, use a ServerCommandContext.
    fn dice_data_with_more_data<F: FnOnce(UserComputationData) -> UserComputationData>(
        &self,
        func: F,
    ) -> UserComputationData {
        let mut data = DiceData::new();
        data.set(self.events.dupe());

        // For commands that don't set a fallback executor config, set a local one.
        set_fallback_executor_config(
            &mut data,
            CommandExecutorConfig {
                executor_kind: CommandExecutorKind::Local(LocalExecutorOptions {}),
                path_separator: PathSeparatorKind::system_default(),
                cache_upload_behavior: CacheUploadBehavior::Disabled,
            },
        );

        let data = UserComputationData {
            data,
            tracker: Arc::new(BuckDiceTracker::new(self.events.dupe())),
            ..Default::default()
        };
        func(data)
    }
}

/// ServerCommandContext provides access to the global daemon state and information about the calling client for
/// the implementation of DaemonApi endpoints (ex. targets, query, build).
pub struct ServerCommandContext {
    pub base_context: BaseServerCommandContext,

    /// The working directory of the client. This is used for resolving things in the request in a
    /// working-dir relative way. For example, it's common to resolve target patterns relative to
    /// the working directory and resolving cell aliases there. This should generally only be used
    /// to interpret values that are in the request. We should convert to client-agnostic things early.
    pub working_dir: ProjectRelativePathBuf,

    /// The oncall specified by the client, if any. This gets injected into request metadata.
    pub oncall: Option<String>,

    config_overrides: Vec<ConfigOverride>,

    host_platform_override: HostPlatformOverride,

    // This ensures that there's only one RE connection during the lifetime of this context. It's possible
    // that we give out other handles, but we don't depend on the lifetimes of those for this guarantee. We
    // also use this to send a RemoteExecutionSessionCreated if the connection is made.
    _re_connection_handle: ReConnectionHandle,

    /// A sender for build signals. This field is exposed to the rest of the command via DICE.
    build_signals: BuildSignalSender,

    /// Starlark profiler instrumentation requested throughout the duration of this command. Usually associated with
    /// the `buck2 profile` command.
    pub starlark_profiler_instrumentation_override: StarlarkProfilerConfiguration,

    record_target_call_stacks: bool,
    disable_starlark_types: bool,

    buck_out_dir: ProjectRelativePathBuf,

    /// Common build options associated with this command.
    build_options: Option<CommonBuildOptions>,

    /// The CellResolver and Configs for this command
    cells_and_configs: AsyncOnceCell<SharedResult<(CellResolver, LegacyBuckConfigs)>>,

    /// Keep emitting heartbeat events while the ServerCommandContext is alive  We put this in an
    /// Option so that we can ensure heartbeat events are cancelled before everything else is
    /// dropped.
    heartbeat_guard_handle: Option<HeartbeatGuard>,

    configure_bxl_file_globals: fn(&mut GlobalsBuilder),

    /// Reuses build config from the previous invocation if there is one
    reuse_current_config: bool,
}

impl ServerCommandContext {
    pub fn new(
        base_context: BaseServerCommandContext,
        client_context: &ClientContext,
        build_signals: BuildSignalSender,
        starlark_profiler_instrumentation_override: StarlarkProfilerConfiguration,
        build_options: Option<&CommonBuildOptions>,
        buck_out_dir: ProjectRelativePathBuf,
        record_target_call_stacks: bool,
        configure_bxl_file_globals: fn(&mut GlobalsBuilder),
    ) -> anyhow::Result<Self> {
        let abs_path = AbsPath::new(&client_context.working_dir)?;

        let project_path = abs_path
            .strip_prefix(base_context.project_root.root())
            .map_err(|_| {
                Into::<anyhow::Error>::into(DaemonCommunicationError::InvalidWorkingDirectory(
                    client_context.working_dir.clone(),
                ))
            })?;

        struct Observer {
            events: EventDispatcher,
        }

        impl ReConnectionObserver for Observer {
            fn session_created(&self, client: &RemoteExecutionClient) {
                let session_id = client.get_session_id();
                let experiment_name = match client.get_experiment_name() {
                    Ok(Some(exp)) => exp,
                    Ok(None) => "".to_owned(),
                    Err(e) => {
                        tracing::debug!("Failed to access RE experiment name: {:#}", e);
                        "<ffi error>".to_owned()
                    }
                };

                self.events
                    .instant_event(buck2_data::RemoteExecutionSessionCreated {
                        session_id: session_id.to_owned(),
                        experiment_name,
                    })
            }
        }

        let mut re_connection_handle = base_context.re_client_manager.get_re_connection();

        re_connection_handle.set_observer(Arc::new(Observer {
            events: base_context.events.dupe(),
        }));

        let oncall = if client_context.oncall.is_empty() {
            None
        } else {
            Some(client_context.oncall.clone())
        };

        let heartbeat_guard_handle = HeartbeatGuard::new(&base_context);

        Ok(ServerCommandContext {
            base_context,
            working_dir: project_path.to_buf().into(),
            config_overrides: client_context.config_overrides.clone(),
            host_platform_override: client_context.host_platform(),
            oncall,
            _re_connection_handle: re_connection_handle,
            build_signals,
            starlark_profiler_instrumentation_override,
            buck_out_dir,
            build_options: build_options.cloned(),
            cells_and_configs: AsyncOnceCell::new(),
            record_target_call_stacks,
            disable_starlark_types: client_context.disable_starlark_types,
            heartbeat_guard_handle: Some(heartbeat_guard_handle),
            configure_bxl_file_globals,
            reuse_current_config: client_context.reuse_current_config,
        })
    }

    pub async fn cells_and_configs(
        &self,
        reuse_current_config: bool,
    ) -> SharedResult<(CellResolver, LegacyBuckConfigs)> {
        self.cells_and_configs
            .get_or_init(async move {
                if reuse_current_config {
                    // TODO this is wrong and racey as the dice manager could evict the current
                    // active dice ctx before the command that reuses config gets there.
                    let dice_ctx = self.base_context.dice_manager.unsafe_dice().ctx();
                    if dice_ctx.is_cell_resolver_key_set().await?
                        && dice_ctx.is_legacy_configs_key_set().await?
                    {
                        return Ok::<(CellResolver, LegacyBuckConfigs), anyhow::Error>((
                            dice_ctx.get_cell_resolver().await?,
                            dice_ctx.get_legacy_configs().await?,
                        ))
                            .shared_error();
                    } else {
                        warn!(
                            "--reuse-current-config flag was set, but there was no previous invocation detected"
                        );
                    }
                }

                let fs = self.project_root();
                let cwd = &self.working_dir;
                parse_legacy_cells(self.config_overrides.iter(), &fs.resolve(cwd), fs)
                    .shared_error()
            })
            .await
            .clone()
    }

    async fn dice_data_constructor(&self) -> DiceCommandDataProvider {
        DiceCommandDataProvider {
            data: self.construct_dice_data().await,
        }
    }

    async fn construct_dice_data(&self) -> anyhow::Result<UserComputationData> {
        let execution_strategy = self
            .build_options
            .as_ref()
            .map(|opts| opts.execution_strategy)
            .map_or(ExecutionStrategy::LocalOnly, |strategy| {
                ExecutionStrategy::from_i32(strategy).expect("execution strategy should be valid")
            });

        let (cell_resolver, legacy_configs) =
            self.cells_and_configs(self.reuse_current_config).await?;

        // TODO(cjhopman): The CellResolver and the legacy configs shouldn't be leaves on the graph. This should
        // just be setting the config overrides and host platform override as leaves on the graph.

        let root_config = legacy_configs
            .get(cell_resolver.root_cell())
            .context("No config for root cell")?;

        let mut run_action_knobs = RunActionKnobs {
            hash_all_commands: self.base_context.hash_all_commands,
            declare_match_in_depfiles: self.base_context.declare_match_in_depfiles,
            ..Default::default()
        };

        if let Some(build_options) = self.build_options.as_ref() {
            run_action_knobs.eager_dep_files = build_options.eager_dep_files;
        }

        let config_threads = root_config.parse("build", "threads")?.unwrap_or(0);

        let concurrency = self
            .build_options
            .as_ref()
            .and_then(|opts| opts.concurrency.as_ref())
            .map_or_else(
                || parse_concurrency(config_threads),
                |obj| parse_concurrency(obj.concurrency),
            )?;

        let executor_global_knobs = ExecutorGlobalKnobs {
            declare_in_local_executor: self.base_context.declare_in_local_executor,
        };

        let executor_config =
            get_executor_config_for_strategy(execution_strategy, self.host_platform_override);
        let blocking_executor: Arc<_> = self.base_context.blocking_executor.dupe();
        let materializer = self.base_context.materializer.dupe();
        let re_connection = self.get_re_connection();
        let build_signals = self.build_signals.dupe();
        let host_sharing_broker =
            HostSharingBroker::new(HostSharingStrategy::SmallerTasksFirst, concurrency);
        let forkserver = self.base_context.forkserver.dupe();

        let upload_all_actions = self
            .build_options
            .as_ref()
            .map_or(false, |opts| opts.upload_all_actions);

        Ok(self.base_context.dice_data_with_more_data(move |mut data| {
            set_fallback_executor_config(&mut data.data, executor_config);
            data.set_command_executor(box CommandExecutorFactory::new(
                re_connection,
                host_sharing_broker,
                materializer.dupe(),
                blocking_executor.dupe(),
                execution_strategy,
                executor_global_knobs,
                upload_all_actions,
                forkserver,
            ));
            data.set_blocking_executor(blocking_executor);
            data.set_materializer(materializer);
            data.set_build_signals(build_signals);
            data.set_run_action_knobs(run_action_knobs);
            data.spawner = Arc::new(BuckSpawner::default());
            data
        }))
    }

    async fn dice_updater(&self) -> anyhow::Result<DiceCommandUpdater> {
        let (cell_resolver, legacy_configs) =
            self.cells_and_configs(self.reuse_current_config).await?;
        // TODO(cjhopman): The CellResolver and the legacy configs shouldn't be leaves on the graph. This should
        // just be setting the config overrides and host platform override as leaves on the graph.

        let cell_alias_resolver = cell_resolver.root_cell_instance().cell_alias_resolver();

        let (interpreter_platform, interpreter_architecture) =
            host_info::get_host_info(self.host_platform_override);
        let configuror = BuildInterpreterConfiguror::new(
            Some(prelude_path(cell_alias_resolver)?),
            interpreter_platform,
            interpreter_architecture,
            self.record_target_call_stacks,
            configure_build_file_globals,
            configure_extension_file_globals,
            self.configure_bxl_file_globals,
            None,
            Arc::new(ConfiguredGraphQueryEnvironment::functions()),
        );

        Ok(DiceCommandUpdater {
            file_watcher: self.base_context.file_watcher.dupe(),
            buck_out_dir: self.buck_out_dir.clone(),
            cell_resolver,
            configuror,
            legacy_configs,
            starlark_profiler_instrumentation_override: self
                .starlark_profiler_instrumentation_override
                .dupe(),
            disable_starlark_types: self.disable_starlark_types,
        })
    }

    pub fn get_re_connection(&self) -> ReConnectionHandle {
        self.base_context.re_client_manager.get_re_connection()
    }
}

struct DiceCommandDataProvider {
    data: anyhow::Result<UserComputationData>,
}

#[async_trait]
impl DiceDataProvider for DiceCommandDataProvider {
    async fn provide(
        self: Box<Self>,
        _ctx: &DiceComputations,
    ) -> anyhow::Result<UserComputationData> {
        self.data
    }
}

struct DiceCommandUpdater {
    file_watcher: Arc<dyn FileWatcher>,
    buck_out_dir: ProjectRelativePathBuf,
    cell_resolver: CellResolver,
    configuror: Arc<dyn InterpreterConfiguror>,
    legacy_configs: LegacyBuckConfigs,
    starlark_profiler_instrumentation_override: StarlarkProfilerConfiguration,
    disable_starlark_types: bool,
}

#[async_trait]
impl DiceUpdater for DiceCommandUpdater {
    async fn update(&self, ctx: DiceTransaction) -> anyhow::Result<DiceTransaction> {
        let ctx = self.file_watcher.sync(ctx).await?;

        ctx.set_buck_out_path(Some(self.buck_out_dir.clone()))?;

        setup_interpreter(
            &ctx,
            self.cell_resolver.dupe(),
            self.configuror.dupe(),
            self.legacy_configs.dupe(),
            self.starlark_profiler_instrumentation_override.dupe(),
            self.disable_starlark_types,
        )?;

        Ok(ctx)
    }
}

impl Drop for ServerCommandContext {
    fn drop(&mut self) {
        // Ensure we cancel the heartbeat guard first.
        std::mem::drop(self.heartbeat_guard_handle.take());
    }
}

#[async_trait]
impl ServerCommandContextTrait for ServerCommandContext {
    fn working_dir(&self) -> &ProjectRelativePath {
        &self.working_dir
    }

    fn project_root(&self) -> &ProjectRoot {
        &self.base_context.project_root
    }

    /// Provides a DiceTransaction, initialized on first use and shared after initialization.
    async fn dice_accessor(&self, _private: PrivateStruct) -> SharedResult<DiceAccessor> {
        Ok(DiceAccessor {
            dice_handler: self.base_context.dice_manager.dupe(),
            data: box self.dice_data_constructor().await,
            setup: box self.dice_updater().await?,
        })
    }

    fn events(&self) -> &EventDispatcher {
        &self.base_context.events
    }

    fn stdout(&mut self) -> anyhow::Result<RawOuputGuard<'_>> {
        // Buffer until MESSAGE_BUFFER_SIZE bytes get written to save gRPC communication overheads
        Ok(RawOuputGuard {
            _phantom: PhantomData,
            inner: BufWriter::with_capacity(4096, RawOutputWriter::new(self)?),
        })
    }

    /// Gathers metadata to attach to events for when a command starts and stops.
    async fn request_metadata(&self) -> anyhow::Result<HashMap<String, String>> {
        // Facebook only: metadata collection for Scribe writes
        facebook_only();

        let mut metadata = metadata::collect();

        metadata.insert(
            "io_provider".to_owned(),
            self.base_context.io.name().to_owned(),
        );

        if let Some(oncall) = &self.oncall {
            metadata.insert("oncall".to_owned(), oncall.clone());
        }

        Ok(metadata)
    }

    /// Gathers metadata from buckconfig to attach to events for when a command enters the critical
    /// section
    async fn config_metadata(&self) -> anyhow::Result<HashMap<String, String>> {
        // Facebook only: metadata collection for Scribe writes
        facebook_only();

        fn add_config(
            map: &mut HashMap<String, String>,
            cfg: &LegacyBuckConfig,
            section: &'static str,
            key: &'static str,
            field_name: &'static str,
        ) {
            if let Some(value) = cfg.get(section, key) {
                map.insert(field_name.to_owned(), value.to_owned());
            }
        }

        fn extract_scuba_defaults(
            config: Option<&LegacyBuckConfig>,
        ) -> Option<serde_json::Map<String, serde_json::Value>> {
            let config = config?.get("scuba", "defaults")?;
            let unescaped_config = shlex::split(config)?.join("");
            let sample_json: serde_json::Value = serde_json::from_str(&unescaped_config).ok()?;
            sample_json.get("normals")?.as_object().cloned()
        }

        let mut metadata = HashMap::new();
        // In the case of invalid configuration (e.g. something like buck2 build -c X), `dice_ctx_default` returns an
        // error. We won't be able to get configs to log in that case, but we shouldn't crash.
        let (cells, configs) = self.cells_and_configs(self.reuse_current_config).await?;
        let root_cell_config = configs.get(cells.root_cell());
        if let Ok(config) = root_cell_config {
            add_config(&mut metadata, config, "log", "repository", "repository");

            // Buck1 honors a configuration field, `scuba.defaults`, by drawing values from the configuration value and
            // inserting them verbatim into Scuba samples. Buck2 doesn't write to Scuba in the same way that Buck1
            // does, but metadata in this function indirectly makes its way to Scuba, so it makes sense to respect at
            // least some of the data within it.
            //
            // The configuration field is expected to be the canonical JSON representation for a Scuba sample, which is
            // to say something like this:
            // ```
            // {
            //   "normals": { "key": "value" },
            //   "ints": { "key": 0 },
            // }
            // ```
            //
            // TODO(swgillespie) - This only covers the normals since Buck2's event protocol only allows for string
            // metadata. Depending on what sort of things we're missing by dropping int default columns, we might want
            // to consider adding support to the protocol for integer metadata.

            if let Ok(cwd_cell_name) = cells.find(&self.working_dir) {
                let cwd_cell_config = configs.get(cwd_cell_name).ok();
                if let Some(normals_obj) = extract_scuba_defaults(cwd_cell_config) {
                    for (key, value) in normals_obj.iter() {
                        if let Some(value) = value.as_str() {
                            metadata.insert(key.clone(), value.to_owned());
                        }
                    }
                }

                // `client.id` is often set via the `-c` flag; `-c` configuration is assigned to the cwd cell and not
                // the root cell.
                if let Some(config) = cwd_cell_config {
                    add_config(&mut metadata, config, "client", "id", "client");
                    add_config(
                        &mut metadata,
                        config,
                        "cache",
                        "schedule_type",
                        "schedule_type",
                    );
                }
            }
        }

        Ok(metadata)
    }

    fn log_target_pattern(&self, providers_patterns: &[ParsedPattern<ProvidersPattern>]) {
        let patterns = providers_patterns.map(|pat| buck2_data::TargetPattern {
            value: format!("{}", pat),
        });

        self.events()
            .instant_event(buck2_data::ResolvedTargetPatterns {
                target_patterns: patterns,
            })
    }
}
