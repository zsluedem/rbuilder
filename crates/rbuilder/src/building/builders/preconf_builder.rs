use crate::{
    building::{
        builders::{dummy_order::DummyOrderFactory, LiveBuilderInput},
        BlockBuildingContext, BlockState, BuiltBlockTrace, PartialBlock,
    },
    primitives::OrderId,
    telemetry,
    utils::{is_provider_factory_health_error, Signer},
};
use ahash::{HashMap, HashSet};
use alloy_primitives::{utils::format_ether, Address};
use reth::providers::{BlockNumReader, ProviderFactory};
use reth_db::database::Database;
use reth_provider::StateProvider;
use serde::Deserialize;

use crate::{
    building::tracers::GasUsedSimulationTracer, live_builder::bidding::SlotBidder,
    roothash::RootHashMode, utils::check_provider_factory_health,
};
use reth::tasks::pool::BlockingTaskPool;
use reth_payload_builder::database::CachedReads;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use time::OffsetDateTime;
use tracing::{debug, error, info, trace, warn};

use super::{
    finalize_block_execution, Block, BlockBuildingAlgorithm, BlockBuildingAlgorithmInput,
    BlockBuildingSink,
};

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PreconfBuilderConfig {
    #[serde(default)]
    pub coinbase_payment: bool,
    /// Amount of time allocated for EVM execution while building block.
    #[serde(default)]
    pub build_duration_deadline_ms: Option<u64>,

    pub dummy_tx_private_key: String,
}

impl PreconfBuilderConfig {
    pub fn build_duration_deadline(&self) -> Option<Duration> {
        self.build_duration_deadline_ms.map(Duration::from_millis)
    }

    pub fn dummy_signer(&self) -> Signer {
        Signer::try_from_secret(self.dummy_tx_private_key.parse().expect("parse secret key"))
            .expect("signer")
    }
}
#[derive(Debug)]
pub struct PreconfBuilderContext<DB> {
    provider_factory: ProviderFactory<DB>,
    root_hash_task_pool: BlockingTaskPool,
    builder_name: String,
    ctx: BlockBuildingContext,
    root_hash_mode: RootHashMode,
    slot_bidder: Arc<dyn SlotBidder>,

    // caches
    cached_reads: Option<CachedReads>,

    // scratchpad
    failed_orders: HashSet<OrderId>,
    order_attempts: HashMap<OrderId, usize>,

    config: PreconfBuilderConfig,
}

impl<DB: Database + Clone + 'static> PreconfBuilderContext<DB> {
    pub fn new(
        provider_factory: ProviderFactory<DB>,
        slot_bidder: Arc<dyn SlotBidder>,
        root_hash_task_pool: BlockingTaskPool,
        builder_name: String,
        ctx: BlockBuildingContext,
        config: PreconfBuilderConfig,
    ) -> Self {
        Self {
            provider_factory,
            root_hash_task_pool,
            builder_name,
            ctx,
            root_hash_mode: RootHashMode::CorrectRoot,
            slot_bidder,
            cached_reads: None,
            failed_orders: HashSet::default(),
            order_attempts: HashMap::default(),
            config,
        }
    }

    pub fn build_block(&mut self) -> eyre::Result<Option<Block>> {
        check_provider_factory_health(self.ctx.block(), &self.provider_factory)?;

        let build_start = Instant::now();
        let orders_closed_at = OffsetDateTime::now_utc();

        // Create a new ctx to remove builder_signer if necessary
        let mut new_ctx = self.ctx.clone();
        new_ctx.modify_use_suggested_fee_recipient_as_coinbase();
        let ctx = &new_ctx;

        self.failed_orders.clear();
        self.order_attempts.clear();

        // @Maybe an issue - we have 2 db txs here (one for hash and one for finalize)
        let state_provider = self
            .provider_factory
            .history_by_block_hash(ctx.attributes.parent)?;

        let fee_recipient_balance_before = state_provider
            .account_balance(ctx.attributes.suggested_fee_recipient)?
            .unwrap_or_default();

        let (mut built_block_trace, state, partial_block) = {
            let mut partial_block =
                PartialBlock::new(false, None).with_tracer(GasUsedSimulationTracer::default());
            let mut state = BlockState::new(&state_provider)
                .with_cached_reads(self.cached_reads.take().unwrap_or_default());
            partial_block.pre_block_call(ctx, &mut state)?;
            let mut built_block_trace = BuiltBlockTrace::new();

            let dummy_signer = self.config.dummy_signer();
            info!(
                "generate tx send from {:?}, and coinbase is {:?}",
                dummy_signer.address, ctx.block_env.coinbase
            );
            let current_nonce = state.nonce(dummy_signer.address)?;
            let mut dummy_order_factory =
                DummyOrderFactory::new(dummy_signer, ctx.chain_spec.chain().id(), current_nonce, ctx.block_env.coinbase);

            // @Perf when gas left is too low we should break.
            for _ in 0..10 {
                if let Some(deadline) = self.config.build_duration_deadline() {
                    if build_start.elapsed() > deadline {
                        break;
                    }
                }
                let tx = dummy_order_factory.generate_tx(ctx.block_env.basefee);

                let start_time = Instant::now();
                let commit_result = partial_block.commit_tx(&mut state, tx, &ctx);
                let order_commit_time = start_time.elapsed();
                let mut gas_used = 0;
                let mut execution_error = None;
                let success = commit_result.is_ok();
                match commit_result {
                    Ok(res) => {
                        gas_used = res.gas_used;
                        built_block_trace.add_included_order(res);
                    }
                    Err(err) => {
                        execution_error = Some(err);
                    }
                }
                dummy_order_factory.increase_nonce();
                trace!(
                    success,
                    order_commit_time_mus = order_commit_time.as_micros(),
                    gas_used,
                    ?execution_error,
                    "Executed order"
                );
            }

            let fee_recipient_balance_after = state_provider
                .account_balance(ctx.attributes.suggested_fee_recipient)?
                .unwrap_or_default();

            let fee_recipient_balance_diff = fee_recipient_balance_after
                .checked_sub(fee_recipient_balance_before)
                .unwrap_or_default();

            let should_finalize = finalize_block_execution(
                ctx,
                &mut partial_block,
                &mut state,
                &mut built_block_trace,
                None,
                self.slot_bidder.as_ref(),
                fee_recipient_balance_diff,
            )?;

            if !should_finalize {
                trace!(
                    block = ctx.block_env.number.to::<u64>(),
                    builder_name = self.builder_name,
                    "Skipped block finalization",
                );
                return Ok(None);
            }

            (built_block_trace, state, partial_block)
        };

        let build_time = build_start.elapsed();

        built_block_trace.fill_time = build_time;

        let start = Instant::now();

        let sim_gas_used = partial_block.tracer.used_gas;
        let finalized_block = partial_block.finalize(
            state,
            ctx,
            self.provider_factory.clone(),
            self.root_hash_mode,
            self.root_hash_task_pool.clone(),
        )?;
        built_block_trace.update_orders_timestamps_after_block_sealed(orders_closed_at);

        self.cached_reads = Some(finalized_block.cached_reads);

        let finalize_time = start.elapsed();

        built_block_trace.finalize_time = finalize_time;

        let txs = finalized_block.sealed_block.body.len();
        let gas_used = finalized_block.sealed_block.gas_used;
        let blobs = finalized_block.txs_blob_sidecars.len();

        telemetry::add_built_block_metrics(
            build_time,
            finalize_time,
            txs,
            blobs,
            gas_used,
            sim_gas_used,
            &self.builder_name,
            ctx.timestamp(),
        );

        trace!(
            block = ctx.block_env.number.to::<u64>(),
            build_time_mus = build_time.as_micros(),
            finalize_time_mus = finalize_time.as_micros(),
            profit = format_ether(built_block_trace.bid_value),
            builder_name = self.builder_name,
            txs,
            blobs,
            gas_used,
            sim_gas_used,
            "Built block",
        );

        Ok(Some(Block {
            trace: built_block_trace,
            sealed_block: finalized_block.sealed_block,
            txs_blobs_sidecars: finalized_block.txs_blob_sidecars,
            builder_name: self.builder_name.clone(),
        }))
    }
}
#[derive(Debug)]
pub struct PreconfBuilderAlgorithm {
    root_hash_task_pool: BlockingTaskPool,
    sbundle_mergeabe_signers: Vec<Address>,
    config: PreconfBuilderConfig,
    name: String,
}

impl PreconfBuilderAlgorithm {
    pub fn new(
        root_hash_task_pool: BlockingTaskPool,
        sbundle_mergeabe_signers: Vec<Address>,
        config: PreconfBuilderConfig,
        name: String,
    ) -> Self {
        Self {
            root_hash_task_pool,
            sbundle_mergeabe_signers,
            config,
            name,
        }
    }
}
impl<DB: Database + Clone + 'static, SinkType: BlockBuildingSink>
    BlockBuildingAlgorithm<DB, SinkType> for PreconfBuilderAlgorithm
{
    fn name(&self) -> String {
        self.name.clone()
    }

    fn build_blocks(&self, input: BlockBuildingAlgorithmInput<DB, SinkType>) {
        let live_input = LiveBuilderInput {
            provider_factory: input.provider_factory,
            root_hash_task_pool: self.root_hash_task_pool.clone(),
            ctx: input.ctx.clone(),
            input: input.input,
            sink: input.sink,
            builder_name: self.name.clone(),
            slot_bidder: input.slot_bidder,
            cancel: input.cancel,
            sbundle_mergeabe_signers: self.sbundle_mergeabe_signers.clone(),
        };
        run_preconf_builder(live_input, &self.config);
    }
}

fn run_preconf_builder<DB: Database + Clone + 'static, SinkType: BlockBuildingSink>(
    input: LiveBuilderInput<DB, SinkType>,
    config: &PreconfBuilderConfig,
) {
    let block_number = input.ctx.block_env.number.to::<u64>();

    let mut builder = PreconfBuilderContext::new(
        input.provider_factory.clone(),
        input.slot_bidder,
        input.root_hash_task_pool,
        input.builder_name,
        input.ctx,
        config.clone(),
    );

    match builder.build_block() {
        Ok(Some(block)) => {
            input.sink.new_block(block);
        }
        Ok(None) => {}
        Err(err) => {
            // @Types
            let err_str = err.to_string();
            if err_str.contains("failed to initialize consistent view") {
                let last_block_number = input
                    .provider_factory
                    .last_block_number()
                    .unwrap_or_default();
                debug!(
                    block_number,
                    last_block_number, "Can't build on this head, cancelling slot"
                );
                input.cancel.cancel();
            } else if !err_str.contains("Profit too low") {
                if is_provider_factory_health_error(&err) {
                    error!(?err, "Cancelling building due to provider factory error");
                } else {
                    warn!(?err, "Error filling orders");
                }
            }
        }
    }
}
