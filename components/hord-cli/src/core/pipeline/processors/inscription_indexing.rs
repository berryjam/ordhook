use std::{
    collections::BTreeMap,
    sync::Arc,
    thread::{sleep, JoinHandle},
    time::Duration,
};

use chainhook_sdk::{
    types::{BitcoinBlockData, TransactionIdentifier},
    utils::Context,
};
use crossbeam_channel::{Sender, TryRecvError};
use rusqlite::Transaction;

use dashmap::DashMap;
use fxhash::FxHasher;
use rusqlite::Connection;
use std::hash::BuildHasherDefault;

use crate::{
    core::{
        pipeline::processors::block_ingestion::store_compacted_blocks,
        protocol::{
            inscription_parsing::get_inscriptions_revealed_in_block,
            inscription_sequencing::{
                augment_block_with_ordinals_inscriptions_data_and_write_to_db_tx,
                parallelize_inscription_data_computations, SequenceCursor,
            },
            inscription_tracking::augment_block_with_ordinals_transfer_data,
        },
        HordConfig,
    },
    db::{get_any_entry_in_ordinal_activities, open_readonly_hord_db_conn},
};

use crate::db::{LazyBlockTransaction, TraversalResult};

use crate::{
    config::Config,
    core::{
        new_traversals_lazy_cache,
        pipeline::{PostProcessorCommand, PostProcessorController, PostProcessorEvent},
    },
    db::{open_readwrite_hord_db_conn, open_readwrite_hord_db_conn_rocks_db},
};

pub fn start_inscription_indexing_processor(
    config: &Config,
    ctx: &Context,
    post_processor: Option<Sender<BitcoinBlockData>>,
) -> PostProcessorController {
    let (commands_tx, commands_rx) = crossbeam_channel::bounded::<PostProcessorCommand>(2);
    let (events_tx, events_rx) = crossbeam_channel::unbounded::<PostProcessorEvent>();

    let config = config.clone();
    let ctx = ctx.clone();
    let handle: JoinHandle<()> = hiro_system_kit::thread_named("Inscription indexing runloop")
        .spawn(move || {
            let cache_l2 = Arc::new(new_traversals_lazy_cache(1024));
            let garbage_collect_every_n_blocks = 100;
            let mut garbage_collect_nth_block = 0;

            let mut inscriptions_db_conn_rw =
                open_readwrite_hord_db_conn(&config.expected_cache_path(), &ctx).unwrap();
            let hord_config = config.get_hord_config();
            let blocks_db_rw =
                open_readwrite_hord_db_conn_rocks_db(&config.expected_cache_path(), &ctx).unwrap();
            let mut empty_cycles = 0;

            let inscriptions_db_conn =
                open_readonly_hord_db_conn(&config.expected_cache_path(), &ctx).unwrap();
            let mut sequence_cursor = SequenceCursor::new(inscriptions_db_conn);

            if let Ok(PostProcessorCommand::Start) = commands_rx.recv() {
                info!(ctx.expect_logger(), "Start inscription indexing runloop");
            }

            loop {
                let (compacted_blocks, mut blocks) = match commands_rx.try_recv() {
                    Ok(PostProcessorCommand::ProcessBlocks(compacted_blocks, blocks)) => {
                        empty_cycles = 0;
                        (compacted_blocks, blocks)
                    }
                    Ok(PostProcessorCommand::Terminate) => break,
                    Ok(PostProcessorCommand::Start) => continue,
                    Err(e) => match e {
                        TryRecvError::Empty => {
                            empty_cycles += 1;
                            if empty_cycles == 10 {
                                empty_cycles = 0;
                                let _ = events_tx.send(PostProcessorEvent::EmptyQueue);
                            }
                            sleep(Duration::from_secs(1));
                            continue;
                        }
                        _ => {
                            break;
                        }
                    },
                };

                // Early return
                if blocks.is_empty() {
                    store_compacted_blocks(compacted_blocks, &blocks_db_rw, &ctx);
                    continue;
                } else {
                    store_compacted_blocks(compacted_blocks, &blocks_db_rw, &Context::empty());
                }

                info!(ctx.expect_logger(), "Processing {} blocks", blocks.len());

                blocks = process_blocks(
                    &mut blocks,
                    &mut sequence_cursor,
                    &cache_l2,
                    &mut inscriptions_db_conn_rw,
                    &hord_config,
                    &post_processor,
                    &ctx,
                );

                garbage_collect_nth_block += blocks.len();

                // Clear L2 cache on a regular basis
                if garbage_collect_nth_block > garbage_collect_every_n_blocks {
                    info!(
                        ctx.expect_logger(),
                        "Clearing cache L2 ({} entries)",
                        cache_l2.len()
                    );
                    cache_l2.clear();
                    garbage_collect_nth_block = 0;
                }
            }
        })
        .expect("unable to spawn thread");

    PostProcessorController {
        commands_tx,
        events_rx,
        thread_handle: handle,
    }
}

pub fn process_blocks(
    next_blocks: &mut Vec<BitcoinBlockData>,
    sequence_cursor: &mut SequenceCursor,
    cache_l2: &Arc<DashMap<(u32, [u8; 8]), LazyBlockTransaction, BuildHasherDefault<FxHasher>>>,
    inscriptions_db_conn_rw: &mut Connection,
    hord_config: &HordConfig,
    post_processor: &Option<Sender<BitcoinBlockData>>,
    ctx: &Context,
) -> Vec<BitcoinBlockData> {
    let mut cache_l1 = BTreeMap::new();

    let mut updated_blocks = vec![];

    for _cursor in 0..next_blocks.len() {
        let inscriptions_db_tx: rusqlite::Transaction<'_> =
            inscriptions_db_conn_rw.transaction().unwrap();

        let mut block = next_blocks.remove(0);

        // We check before hand if some data were pre-existing, before processing
        // Always discard if we have some existing content at this block height (inscription or transfers)
        let any_existing_activity = get_any_entry_in_ordinal_activities(
            &block.block_identifier.index,
            &inscriptions_db_tx,
            ctx,
        );

        let _ = process_block(
            &mut block,
            &next_blocks,
            sequence_cursor,
            &mut cache_l1,
            cache_l2,
            &inscriptions_db_tx,
            hord_config,
            ctx,
        );

        let inscriptions_revealed = get_inscriptions_revealed_in_block(&block)
            .iter()
            .map(|d| d.inscription_number.to_string())
            .collect::<Vec<String>>();

        ctx.try_log(|logger| {
            info!(
                logger,
                "Block #{} processed and revealed {} inscriptions [{}]",
                block.block_identifier.index,
                inscriptions_revealed.len(),
                inscriptions_revealed.join(", ")
            )
        });

        if any_existing_activity {
            ctx.try_log(|logger| {
                warn!(
                    logger,
                    "Dropping updates for block #{}, activities present in database",
                    block.block_identifier.index,
                )
            });
            let _ = inscriptions_db_tx.rollback();
        } else {
            match inscriptions_db_tx.commit() {
                Ok(_) => {
                    // ctx.try_log(|logger| {
                    //     info!(
                    //         logger,
                    //         "Updates saved for block {}", block.block_identifier.index,
                    //     )
                    // });
                }
                Err(e) => {
                    ctx.try_log(|logger| {
                        error!(
                            logger,
                            "Unable to update changes in block #{}: {}",
                            block.block_identifier.index,
                            e.to_string()
                        )
                    });
                }
            }
        }

        if let Some(post_processor_tx) = post_processor {
            let _ = post_processor_tx.send(block.clone());
        }
        updated_blocks.push(block);
    }
    updated_blocks
}

pub fn process_block(
    block: &mut BitcoinBlockData,
    next_blocks: &Vec<BitcoinBlockData>,
    sequence_cursor: &mut SequenceCursor,
    cache_l1: &mut BTreeMap<(TransactionIdentifier, usize), TraversalResult>,
    cache_l2: &Arc<DashMap<(u32, [u8; 8]), LazyBlockTransaction, BuildHasherDefault<FxHasher>>>,
    inscriptions_db_tx: &Transaction,
    hord_config: &HordConfig,
    ctx: &Context,
) -> Result<(), String> {
    let any_processable_transactions = parallelize_inscription_data_computations(
        &block,
        &next_blocks,
        cache_l1,
        cache_l2,
        inscriptions_db_tx,
        &hord_config,
        ctx,
    )?;

    if !any_processable_transactions {
        return Ok(());
    }

    let inner_ctx = if hord_config.logs.ordinals_internals {
        ctx.clone()
    } else {
        Context::empty()
    };

    // Handle inscriptions
    let _ = augment_block_with_ordinals_inscriptions_data_and_write_to_db_tx(
        block,
        sequence_cursor,
        cache_l1,
        &inscriptions_db_tx,
        &inner_ctx,
    );

    // Handle transfers
    let _ = augment_block_with_ordinals_transfer_data(block, inscriptions_db_tx, true, &inner_ctx);

    Ok(())
}