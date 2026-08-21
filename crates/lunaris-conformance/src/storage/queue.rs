//! Plan 05-02 D-17 — `publish` / `subscribe` round-trip conformance.
//!
//! Subscribes to a unique conformance topic FIRST, then publishes a
//! known payload, then waits up to 5 seconds (`tokio::time::timeout`)
//! for the payload to arrive on the subscription. The 5-second cap is
//! the T-05-02-03 mitigation in the threat model — a stuck broker
//! doesn't hang CI.
//!
//! Caller MUST gate on `storage.capabilities().queue_native == true`;
//! [`run_full_storage_suite`](crate::storage::run_full_storage_suite)
//! handles that gate.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use lunaris_core::storage::StoragePort;

use crate::suite_scope::SuiteScope;

const CONFORMANCE_TOPIC: &str = "__conformance_round_trip__";
const CONFORMANCE_GROUP: &str = "conformance-rt";
const ROUND_TRIP_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn publish_subscribe_round_trip(
    storage: &Arc<dyn StoragePort>,
    run: &SuiteScope,
) -> anyhow::Result<()> {
    debug_assert!(storage.capabilities().queue_native, "caller must gate on queue_native");

    let payload: Bytes = b"conformance:round-trip-payload".to_vec().into();

    // Subscribe BEFORE publishing — Moon's MQ.POP is a polling stream so
    // pre-subscriber publishes are still durable, but ordering this way
    // also lets the test pass on hypothetical brokers that drop pre-
    // subscriber messages.
    let mut stream =
        storage.subscribe(run.scope(), CONFORMANCE_GROUP, CONFORMANCE_TOPIC, 0).await?;

    let _offset = storage.publish(run.scope(), CONFORMANCE_TOPIC, 0, payload.clone()).await?;

    let result = tokio::time::timeout(ROUND_TRIP_TIMEOUT, async {
        while let Some(msg) = stream.next().await {
            let env = msg?;
            // Compare payload bytes; envelope shape (offset / topic /
            // partition) varies per backend but `payload` is the
            // canonical equality field.
            if env.payload == payload {
                return Ok::<(), anyhow::Error>(());
            }
        }
        anyhow::bail!(
            "queue::publish_subscribe_round_trip: subscribe stream ended before payload arrived"
        )
    })
    .await;

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => anyhow::bail!(
            "queue::publish_subscribe_round_trip: timed out after {:?} waiting for payload",
            ROUND_TRIP_TIMEOUT,
        ),
    }
}
