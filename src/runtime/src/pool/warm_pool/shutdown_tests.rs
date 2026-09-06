use super::*;

#[tokio::test]
async fn drain_idle_signals_and_joins_in_progress_maintenance() {
    let pool = WarmPool::start(
        PoolConfig {
            min_idle: 0,
            max_size: 1,
            ..PoolConfig::default()
        },
        BoxConfig::default(),
        EventEmitter::new(10),
    )
    .await
    .expect("start an empty pool without a hypervisor");

    pool.signal_shutdown();
    pool.replenish_handle
        .lock()
        .await
        .take()
        .unwrap()
        .await
        .unwrap();
    pool.shutdown_tx.send(false).unwrap();

    let mut shutdown = pool.shutdown_rx.clone();
    let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    *pool.replenish_handle.lock().await = Some(tokio::spawn(async move {
        shutdown.wait_for(|stop| *stop).await.unwrap();
        observed_tx.send(()).unwrap();
        // Model the cleanup of a VM whose boot finished during shutdown.
        finish_rx.await.unwrap();
    }));

    let mut draining = Box::pin(pool.drain_idle());
    tokio::select! {
        result = &mut draining => {
            panic!("drain returned before maintenance cleanup finished: {result:?}");
        }
        observed = tokio::time::timeout(Duration::from_secs(5), observed_rx) => {
            observed.expect("drain must signal shutdown").unwrap();
        }
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut draining)
            .await
            .is_err(),
        "drain must retain maintenance ownership while its cleanup is pending"
    );
    finish_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), draining)
        .await
        .expect("drain completes after maintenance cleanup")
        .unwrap();
    assert!(pool.replenish_handle.lock().await.is_none());
    assert_eq!(pool.idle_count().await, 0);
    assert_eq!(pool.stats().await.idle_count, 0);
}
