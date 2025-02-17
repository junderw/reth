//! Launch the Ethereum node.

use futures::{future::Either, stream, stream_select, StreamExt};
use reth_beacon_consensus::{
    hooks::{EngineHooks, PruneHook, StaticFileHook},
    BeaconConsensusEngineHandle,
};
use reth_ethereum_engine::service::EthService;
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_exex::ExExManagerHandle;
use reth_network::NetworkEvents;
use reth_node_api::{FullNodeTypes, NodeAddOns};
use reth_node_builder::{
    hooks::NodeHooks,
    rpc::{launch_rpc_servers, EthApiBuilderProvider},
    AddOns, ExExLauncher, FullNode, LaunchContext, LaunchNode, NodeAdapter,
    NodeBuilderWithComponents, NodeComponents, NodeComponentsBuilder, NodeHandle, NodeTypesAdapter,
};
use reth_node_core::{
    dirs::{ChainPath, DataDirPath},
    exit::NodeExitFuture,
    rpc::eth::{helpers::AddDevSigners, FullEthApiServer},
    version::{CARGO_PKG_VERSION, CLIENT_CODE, NAME_CLIENT, VERGEN_GIT_SHA},
};
use reth_node_events::{cl::ConsensusLayerHealthEvents, node};
use reth_provider::providers::BlockchainProvider;
use reth_rpc_engine_api::{capabilities::EngineCapabilities, EngineApi};
use reth_rpc_types::engine::ClientVersionV1;
use reth_tasks::TaskExecutor;
use reth_tokio_util::EventSender;
use reth_tracing::tracing::{debug, info};
use std::sync::mpsc::channel;
use tokio::sync::{mpsc::unbounded_channel, oneshot};
use tokio_stream::wrappers::UnboundedReceiverStream;

/// The Ethereum node launcher.
#[derive(Debug)]
pub struct EthNodeLauncher {
    /// The task executor for the node.
    pub ctx: LaunchContext,
}

impl EthNodeLauncher {
    /// Create a new instance of the ethereum node launcher.
    pub const fn new(task_executor: TaskExecutor, data_dir: ChainPath<DataDirPath>) -> Self {
        Self { ctx: LaunchContext::new(task_executor, data_dir) }
    }
}

impl<T, CB, AO> LaunchNode<NodeBuilderWithComponents<T, CB, AO>> for EthNodeLauncher
where
    T: FullNodeTypes<
        Provider = BlockchainProvider<<T as FullNodeTypes>::DB>,
        Engine = EthEngineTypes,
    >,
    CB: NodeComponentsBuilder<T>,
    AO: NodeAddOns<NodeAdapter<T, CB::Components>>,
    AO::EthApi:
        EthApiBuilderProvider<NodeAdapter<T, CB::Components>> + FullEthApiServer + AddDevSigners,
{
    type Node = NodeHandle<NodeAdapter<T, CB::Components>, AO>;

    async fn launch_node(
        self,
        target: NodeBuilderWithComponents<T, CB, AO>,
    ) -> eyre::Result<Self::Node> {
        let Self { ctx } = self;
        let NodeBuilderWithComponents {
            adapter: NodeTypesAdapter { database },
            components_builder,
            add_ons: AddOns { hooks, rpc, exexs: installed_exex },
            config,
        } = target;
        let NodeHooks { on_component_initialized, on_node_started, .. } = hooks;

        // setup the launch context
        let ctx = ctx
            .with_configured_globals()
            // load the toml config
            .with_loaded_toml_config(config)?
            // add resolved peers
            .with_resolved_peers().await?
            // attach the database
            .attach(database.clone())
            // ensure certain settings take effect
            .with_adjusted_configs()
            // Create the provider factory
            .with_provider_factory().await?
            .inspect(|_| {
                info!(target: "reth::cli", "Database opened");
            })
            .with_prometheus_server().await?
            .inspect(|this| {
                debug!(target: "reth::cli", chain=%this.chain_id(), genesis=?this.genesis_hash(), "Initializing genesis");
            })
            .with_genesis()?
            .inspect(|this| {
                info!(target: "reth::cli", "\n{}", this.chain_spec().display_hardforks());
            })
            .with_metrics_task()
            // passing FullNodeTypes as type parameter here so that we can build
            // later the components.
            .with_blockchain_db::<T>()?
            .with_components(components_builder, on_component_initialized).await?;

        // spawn exexs
        let exex_manager_handle = ExExLauncher::new(
            ctx.head(),
            ctx.node_adapter().clone(),
            installed_exex,
            ctx.configs().clone(),
        )
        .launch()
        .await;

        // create pipeline
        let network_client = ctx.components().network().fetch_client().await?;
        let (consensus_engine_tx, consensus_engine_rx) = unbounded_channel();

        let max_block = ctx.max_block(network_client.clone()).await?;
        let mut hooks = EngineHooks::new();

        let static_file_producer = ctx.static_file_producer();
        let static_file_producer_events = static_file_producer.lock().events();
        hooks.add(StaticFileHook::new(
            static_file_producer.clone(),
            Box::new(ctx.task_executor().clone()),
        ));
        info!(target: "reth::cli", "StaticFileProducer initialized");

        // Configure the pipeline
        let pipeline_exex_handle =
            exex_manager_handle.clone().unwrap_or_else(ExExManagerHandle::empty);
        let pipeline = reth_node_builder::setup::build_networked_pipeline(
            &ctx.toml_config().stages,
            network_client.clone(),
            ctx.consensus(),
            ctx.provider_factory().clone(),
            ctx.task_executor(),
            ctx.sync_metrics_tx(),
            ctx.prune_config(),
            max_block,
            static_file_producer,
            ctx.components().block_executor().clone(),
            pipeline_exex_handle,
        )?;

        let pipeline_events = pipeline.events();

        // TODO: support --debug.tip
        let _initial_target = ctx.node_config().debug.tip;

        let mut pruner_builder = ctx.pruner_builder();
        if let Some(exex_manager_handle) = &exex_manager_handle {
            pruner_builder =
                pruner_builder.finished_exex_height(exex_manager_handle.finished_height());
        }
        let pruner = pruner_builder.build_with_provider_factory(ctx.provider_factory().clone());

        let pruner_events = pruner.events();
        info!(target: "reth::cli", prune_config=?ctx.prune_config().unwrap_or_default(), "Pruner initialized");
        hooks.add(PruneHook::new(pruner, Box::new(ctx.task_executor().clone())));

        let (to_tree_tx, _to_tree_rx) = channel();
        let (_from_tree_tx, from_tree_rx) = unbounded_channel();

        // Configure the consensus engine
        let eth_service = EthService::new(
            ctx.chain_spec(),
            network_client.clone(),
            // to tree
            to_tree_tx,
            // from tree
            from_tree_rx,
            UnboundedReceiverStream::new(consensus_engine_rx),
            pipeline,
            Box::new(ctx.task_executor().clone()),
        );

        let event_sender = EventSender::default();

        let beacon_engine_handle =
            BeaconConsensusEngineHandle::new(consensus_engine_tx, event_sender);

        info!(target: "reth::cli", "Consensus engine initialized");

        let events = stream_select!(
            ctx.components().network().event_listener().map(Into::into),
            // TODO get engine events
            pipeline_events.map(Into::into),
            if ctx.node_config().debug.tip.is_none() && !ctx.is_dev() {
                Either::Left(
                    ConsensusLayerHealthEvents::new(Box::new(ctx.blockchain_db().clone()))
                        .map(Into::into),
                )
            } else {
                Either::Right(stream::empty())
            },
            pruner_events.map(Into::into),
            static_file_producer_events.map(Into::into),
        );
        ctx.task_executor().spawn_critical(
            "events task",
            node::handle_events(
                Some(Box::new(ctx.components().network().clone())),
                Some(ctx.head().number),
                events,
                database.clone(),
            ),
        );

        let client = ClientVersionV1 {
            code: CLIENT_CODE,
            name: NAME_CLIENT.to_string(),
            version: CARGO_PKG_VERSION.to_string(),
            commit: VERGEN_GIT_SHA.to_string(),
        };
        let engine_api = EngineApi::new(
            ctx.blockchain_db().clone(),
            ctx.chain_spec(),
            beacon_engine_handle,
            ctx.components().payload_builder().clone().into(),
            Box::new(ctx.task_executor().clone()),
            client,
            EngineCapabilities::default(),
        );
        info!(target: "reth::cli", "Engine API handler initialized");

        // extract the jwt secret from the args if possible
        let jwt_secret = ctx.auth_jwt_secret()?;

        // Start RPC servers
        let (rpc_server_handles, rpc_registry) = launch_rpc_servers(
            ctx.node_adapter().clone(),
            engine_api,
            ctx.node_config(),
            jwt_secret,
            rpc,
        )
        .await?;

        // Run consensus engine to completion
        let (tx, rx) = oneshot::channel();
        info!(target: "reth::cli", "Starting consensus engine");
        ctx.task_executor().spawn_critical_blocking("consensus engine", async move {
            let res = eth_service.await;
            let _ = tx.send(res);
        });

        let full_node = FullNode {
            evm_config: ctx.components().evm_config().clone(),
            block_executor: ctx.components().block_executor().clone(),
            pool: ctx.components().pool().clone(),
            network: ctx.components().network().clone(),
            provider: ctx.node_adapter().provider.clone(),
            payload_builder: ctx.components().payload_builder().clone(),
            task_executor: ctx.task_executor().clone(),
            rpc_server_handles,
            rpc_registry,
            config: ctx.node_config().clone(),
            data_dir: ctx.data_dir().clone(),
        };
        // Notify on node started
        on_node_started.on_event(full_node.clone())?;

        let handle = NodeHandle {
            node_exit_future: NodeExitFuture::new(
                async { Ok(rx.await??) },
                full_node.config.debug.terminate,
            ),
            node: full_node,
        };

        Ok(handle)
    }
}
