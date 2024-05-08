use anyhow::Context as _;
use test_casing::test_casing;
use tracing::Instrument as _;
use zksync_concurrency::{ctx, scope};
use zksync_consensus_network::testonly::{new_configs, new_fullnode};
use zksync_consensus_roles::validator::testonly::{Setup, SetupSpec};
use zksync_types::{L1BatchNumber, L2BlockNumber};

use super::*;
use crate::utils::testonly::Snapshot;

const CHAIN_ID: validator::ChainId = validator::ChainId(1337);

async fn new_pool(from_snapshot: bool) -> ConnectionPool {
    match from_snapshot {
        true => {
            ConnectionPool::from_snapshot(Snapshot::make(L1BatchNumber(23), L2BlockNumber(87), &[]))
                .await
        }
        false => ConnectionPool::from_genesis().await,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_validator_block_store() {
    zksync_concurrency::testonly::abort_on_panic();
    let ctx = &ctx::test_root(&ctx::RealClock);
    let rng = &mut ctx.rng();
    let pool = new_pool(false).await;

    // Fill storage with unsigned L2 blocks.
    // Fetch a suffix of blocks that we will generate (fake) certs for.
    let want = scope::run!(ctx, |ctx, s| async {
        // Start state keeper.
        let (mut sk, runner) = testonly::StateKeeper::new(ctx, pool.clone()).await?;
        s.spawn_bg(runner.run(ctx));
        sk.push_random_blocks(rng, 10).await;
        pool.wait_for_payload(ctx, sk.last_block()).await?;
        let mut setup = SetupSpec::new(rng, 3);
        setup.first_block = validator::BlockNumber(4);
        let mut setup = Setup::from(setup);
        let mut conn = pool.connection(ctx).await.wrap("connection()")?;
        conn.try_update_genesis(ctx, &setup.genesis)
            .await
            .wrap("try_update_genesis()")?;
        for i in setup.genesis.first_block.0..sk.last_block().next().0 {
            let i = validator::BlockNumber(i);
            let payload = conn
                .payload(ctx, i)
                .await
                .wrap(i)?
                .with_context(|| format!("payload for {i:?} not found"))?
                .encode();
            setup.push_block(payload);
        }
        Ok(setup.blocks.clone())
    })
    .await
    .unwrap();

    // Insert blocks one by one and check the storage state.
    for (i, block) in want.iter().enumerate() {
        scope::run!(ctx, |ctx, s| async {
            let (store, runner) = Store::new(ctx, pool.clone(), None).await.unwrap();
            s.spawn_bg(runner.run(ctx));
            let (block_store, runner) =
                BlockStore::new(ctx, Box::new(store.clone())).await.unwrap();
            s.spawn_bg(runner.run(ctx));
            block_store.queue_block(ctx, block.clone()).await.unwrap();
            block_store
                .wait_until_persisted(ctx, block.number())
                .await
                .unwrap();
            let got = pool
                .wait_for_certificates(ctx, block.number())
                .await
                .unwrap();
            assert_eq!(want[..=i], got);
            Ok(())
        })
        .await
        .unwrap();
    }
}

// In the current implementation, consensus certificates are created asynchronously
// for the L2 blocks constructed by the StateKeeper. This means that consensus actor
// is effectively just back filling the consensus certificates for the L2 blocks in storage.
#[test_casing(2, [false, true])]
#[tokio::test(flavor = "multi_thread")]
async fn test_validator(from_snapshot: bool) {
    zksync_concurrency::testonly::abort_on_panic();
    let ctx = &ctx::test_root(&ctx::AffineClock::new(10.));
    let rng = &mut ctx.rng();
    let setup = Setup::new(rng, 1);
    let cfgs = new_configs(rng, &setup, 0);

    scope::run!(ctx, |ctx, s| async {
        tracing::info!("Start state keeper.");
        let pool = new_pool(from_snapshot).await;
        let (mut sk, runner) = testonly::StateKeeper::new(ctx, pool.clone()).await?;
        s.spawn_bg(runner.run(ctx));

        tracing::info!("Populate storage with a bunch of blocks.");
        sk.push_random_blocks(rng, 5).await;
        pool
            .wait_for_payload(ctx, sk.last_block())
            .await
            .context("sk.wait_for_payload(<1st phase>)")?;

        tracing::info!("Restart consensus actor a couple times, making it process a bunch of blocks each time.");
        for iteration in 0..3 {
            tracing::info!("iteration {iteration}");
            scope::run!(ctx, |ctx, s| async {
                tracing::info!("Start consensus actor");
                // In the first iteration it will initialize genesis.
                let (cfg,secrets) = testonly::config(&cfgs[0]);
                s.spawn_bg(run_main_node(ctx, cfg, secrets, pool.clone(), CHAIN_ID));

                tracing::info!("Generate couple more blocks and wait for consensus to catch up.");
                sk.push_random_blocks(rng, 3).await;
                pool
                    .wait_for_certificate(ctx, sk.last_block())
                    .await
                    .context("wait_for_certificate(<2nd phase>)")?;

                tracing::info!("Synchronously produce blocks one by one, and wait for consensus.");
                for _ in 0..2 {
                    sk.push_random_blocks(rng, 1).await;
                    pool
                        .wait_for_certificate(ctx, sk.last_block())
                        .await
                        .context("wait_for_certificate(<3rd phase>)")?;
                }

                tracing::info!("Verify all certificates");
                pool
                    .wait_for_certificates_and_verify(ctx, sk.last_block())
                    .await
                    .context("wait_for_certificates_and_verify()")?;
                Ok(())
            })
            .await
            .context(iteration)?;
        }
        Ok(())
    })
    .await
    .unwrap();
}

// Test running a validator node and 2 full nodes recovered from different snapshots.
#[tokio::test(flavor = "multi_thread")]
async fn test_nodes_from_various_snapshots() {
    zksync_concurrency::testonly::abort_on_panic();
    let ctx = &ctx::test_root(&ctx::AffineClock::new(10.));
    let rng = &mut ctx.rng();
    let setup = Setup::new(rng, 1);
    let validator_cfg = new_configs(rng, &setup, 0).pop().unwrap();

    scope::run!(ctx, |ctx, s| async {
        tracing::info!("spawn validator");
        let validator_pool = ConnectionPool::from_genesis().await;
        let (mut validator, runner) =
            testonly::StateKeeper::new(ctx, validator_pool.clone()).await?;
        s.spawn_bg(runner.run(ctx).instrument(tracing::info_span!("validator")));
        let (cfg, secrets) = testonly::config(&validator_cfg);
        s.spawn_bg(run_main_node(
            ctx,
            cfg,
            secrets,
            validator_pool.clone(),
            CHAIN_ID,
        ));

        tracing::info!("produce some batches");
        validator.push_random_blocks(rng, 5).await;
        validator.seal_batch().await;
        validator_pool
            .wait_for_certificate(ctx, validator.last_block())
            .await?;

        tracing::info!("take snapshot and start a node from it");
        let snapshot = validator_pool.snapshot(ctx).await?;
        let node_pool = ConnectionPool::from_snapshot(snapshot).await;
        let (node, runner) = testonly::StateKeeper::new(ctx, node_pool.clone()).await?;
        s.spawn_bg(runner.run(ctx).instrument(tracing::info_span!("node1")));
        let conn = validator.connect(ctx).await?;
        s.spawn_bg(async {
            let cfg = new_fullnode(&mut ctx.rng(), &validator_cfg);
            node.run_p2p_fetcher(ctx, conn, &cfg).await
        });

        tracing::info!("produce more batches");
        validator.push_random_blocks(rng, 5).await;
        validator.seal_batch().await;
        node_pool
            .wait_for_certificate(ctx, validator.last_block())
            .await?;

        tracing::info!("take another snapshot and start a node from it");
        let snapshot = validator_pool.snapshot(ctx).await?;
        let node_pool2 = ConnectionPool::from_snapshot(snapshot).await;
        let (node, runner) = testonly::StateKeeper::new(ctx, node_pool2.clone()).await?;
        s.spawn_bg(runner.run(ctx).instrument(tracing::info_span!("node2")));
        let conn = validator.connect(ctx).await?;
        s.spawn_bg(async {
            let cfg = new_fullnode(&mut ctx.rng(), &validator_cfg);
            node.run_p2p_fetcher(ctx, conn, &cfg).await
        });

        tracing::info!("produce more blocks and compare storages");
        validator.push_random_blocks(rng, 5).await;
        let want = validator_pool
            .wait_for_certificates_and_verify(ctx, validator.last_block())
            .await?;
        // node stores should be suffixes for validator store.
        for got in [
            node_pool
                .wait_for_certificates_and_verify(ctx, validator.last_block())
                .await?,
            node_pool2
                .wait_for_certificates_and_verify(ctx, validator.last_block())
                .await?,
        ] {
            assert_eq!(want[want.len() - got.len()..], got[..]);
        }
        Ok(())
    })
    .await
    .unwrap();
}

// Test running a validator node and a couple of full nodes.
// Validator is producing signed blocks and fetchers are expected to fetch
// them directly or indirectly.
#[test_casing(2, [false, true])]
#[tokio::test(flavor = "multi_thread")]
async fn test_full_nodes(from_snapshot: bool) {
    const NODES: usize = 2;

    zksync_concurrency::testonly::abort_on_panic();
    let ctx = &ctx::test_root(&ctx::AffineClock::new(10.));
    let rng = &mut ctx.rng();
    let setup = Setup::new(rng, 1);
    let validator_cfgs = new_configs(rng, &setup, 0);

    // topology:
    // validator <-> node <-> node <-> ...
    let mut node_cfgs = vec![];
    for _ in 0..NODES {
        node_cfgs.push(new_fullnode(
            rng,
            node_cfgs.last().unwrap_or(&validator_cfgs[0]),
        ));
    }

    // Run validator and fetchers in parallel.
    scope::run!(ctx, |ctx, s| async {
        let validator_pool = new_pool(from_snapshot).await;
        let (mut validator, runner) =
            testonly::StateKeeper::new(ctx, validator_pool.clone()).await?;
        s.spawn_bg(async {
            runner
                .run(ctx)
                .instrument(tracing::info_span!("validator"))
                .await
                .context("validator")
        });
        tracing::info!("Generate a couple of blocks, before initializing consensus genesis.");
        validator.push_random_blocks(rng, 5).await;
        // API server needs at least 1 L1 batch to start.
        validator.seal_batch().await;
        validator_pool
            .wait_for_payload(ctx, validator.last_block())
            .await
            .unwrap();

        tracing::info!("Run validator.");
        let (cfg, secrets) = testonly::config(&validator_cfgs[0]);
        s.spawn_bg(run_main_node(
            ctx,
            cfg,
            secrets,
            validator_pool.clone(),
            CHAIN_ID,
        ));

        tracing::info!("Run nodes.");
        let mut node_pools = vec![];
        for (i, cfg) in node_cfgs.iter().enumerate() {
            let i = ctx::NoCopy(i);
            let pool = new_pool(from_snapshot).await;
            let (node, runner) = testonly::StateKeeper::new(ctx, pool.clone()).await?;
            node_pools.push(pool.clone());
            s.spawn_bg(async {
                let i = i;
                runner
                    .run(ctx)
                    .instrument(tracing::info_span!("node", i = *i))
                    .await
                    .with_context(|| format!("node{}", *i))
            });
            s.spawn_bg(node.run_p2p_fetcher(ctx, validator.connect(ctx).await?, cfg));
        }

        tracing::info!("Make validator produce blocks and wait for fetchers to get them.");
        // Note that block from before and after genesis have to be fetched.
        validator.push_random_blocks(rng, 5).await;
        let want_last = validator.last_block();
        let want = validator_pool
            .wait_for_certificates_and_verify(ctx, want_last)
            .await?;
        for pool in &node_pools {
            assert_eq!(
                want,
                pool.wait_for_certificates_and_verify(ctx, want_last)
                    .await?
            );
        }
        Ok(())
    })
    .await
    .unwrap();
}

// Test fetcher back filling missing certs.
#[test_casing(2, [false, true])]
#[tokio::test(flavor = "multi_thread")]
async fn test_p2p_fetcher_backfill_certs(from_snapshot: bool) {
    zksync_concurrency::testonly::abort_on_panic();
    let ctx = &ctx::test_root(&ctx::AffineClock::new(10.));
    let rng = &mut ctx.rng();
    let setup = Setup::new(rng, 1);
    let validator_cfg = new_configs(rng, &setup, 0)[0].clone();
    let node_cfg = new_fullnode(rng, &validator_cfg);

    scope::run!(ctx, |ctx, s| async {
        tracing::info!("Spawn validator.");
        let validator_pool = new_pool(from_snapshot).await;
        let (mut validator, runner) =
            testonly::StateKeeper::new(ctx, validator_pool.clone()).await?;
        s.spawn_bg(runner.run(ctx));
        let (cfg, secrets) = testonly::config(&validator_cfg);
        s.spawn_bg(run_main_node(
            ctx,
            cfg,
            secrets,
            validator_pool.clone(),
            CHAIN_ID,
        ));
        // API server needs at least 1 L1 batch to start.
        validator.seal_batch().await;
        let client = validator.connect(ctx).await?;

        let node_pool = new_pool(from_snapshot).await;

        tracing::info!("Run p2p fetcher.");
        scope::run!(ctx, |ctx, s| async {
            let (node, runner) = testonly::StateKeeper::new(ctx, node_pool.clone()).await?;
            s.spawn_bg(runner.run(ctx));
            s.spawn_bg(node.run_p2p_fetcher(ctx, client.clone(), &node_cfg));
            validator.push_random_blocks(rng, 3).await;
            node_pool
                .wait_for_certificate(ctx, validator.last_block())
                .await?;
            Ok(())
        })
        .await
        .unwrap();

        tracing::info!("Run centralized fetcher.");
        scope::run!(ctx, |ctx, s| async {
            let (node, runner) = testonly::StateKeeper::new(ctx, node_pool.clone()).await?;
            s.spawn_bg(runner.run(ctx));
            s.spawn_bg(node.run_centralized_fetcher(ctx, client.clone()));
            validator.push_random_blocks(rng, 3).await;
            node_pool
                .wait_for_payload(ctx, validator.last_block())
                .await?;
            Ok(())
        })
        .await
        .unwrap();

        tracing::info!("Run p2p fetcher again.");
        scope::run!(ctx, |ctx, s| async {
            let (node, runner) = testonly::StateKeeper::new(ctx, node_pool.clone()).await?;
            s.spawn_bg(runner.run(ctx));
            s.spawn_bg(node.run_p2p_fetcher(ctx, client.clone(), &node_cfg));
            validator.push_random_blocks(rng, 3).await;
            let want = validator_pool
                .wait_for_certificates_and_verify(ctx, validator.last_block())
                .await?;
            let got = node_pool
                .wait_for_certificates_and_verify(ctx, validator.last_block())
                .await?;
            assert_eq!(want, got);
            Ok(())
        })
        .await
        .unwrap();
        Ok(())
    })
    .await
    .unwrap();
}

#[test_casing(2, [false, true])]
#[tokio::test]
async fn test_centralized_fetcher(from_snapshot: bool) {
    zksync_concurrency::testonly::abort_on_panic();
    let ctx = &ctx::test_root(&ctx::RealClock);
    let rng = &mut ctx.rng();

    scope::run!(ctx, |ctx, s| async {
        tracing::info!("Spawn a validator.");
        let validator_pool = new_pool(from_snapshot).await;
        let (mut validator, runner) =
            testonly::StateKeeper::new(ctx, validator_pool.clone()).await?;
        s.spawn_bg(runner.run(ctx).instrument(tracing::info_span!("validator")));

        tracing::info!("Produce a batch (to make api server start)");
        // TODO: ensure at least L1 batch in `testonly::StateKeeper::new()` to make it fool proof.
        validator.seal_batch().await;

        tracing::info!("Spawn a node.");
        let node_pool = new_pool(from_snapshot).await;
        let (node, runner) = testonly::StateKeeper::new(ctx, node_pool.clone()).await?;
        s.spawn_bg(runner.run(ctx).instrument(tracing::info_span!("fetcher")));
        s.spawn_bg(node.run_centralized_fetcher(ctx, validator.connect(ctx).await?));

        tracing::info!("Produce some blocks and wait for node to fetch them");
        validator.push_random_blocks(rng, 10).await;
        let want = validator_pool
            .wait_for_payload(ctx, validator.last_block())
            .await?;
        let got = node_pool
            .wait_for_payload(ctx, validator.last_block())
            .await?;
        assert_eq!(want, got);
        Ok(())
    })
    .await
    .unwrap();
}
