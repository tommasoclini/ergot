//! End-to-end tests for two DirectEdge stacks communicating over in-memory streams.
//!
//! These tests use `tokio::io::duplex()` to create bidirectional pipes,
//! connecting a controller stack to a target stack without real hardware.

#![cfg(feature = "tokio-std")]
// Skip under miri — tokio runtime internals trigger false-positive leak detection
#![cfg(not(miri))]

use std::{pin::pin, sync::Arc, time::Duration};

use ergot::{
    interface_manager::{InterfaceState, LivenessConfig, Profile},
    toolkits::tokio_stream::{
        self as stream_kit, EdgeStack, WaitQueue, register_controller_stream,
        register_target_stream,
    },
    well_known::ErgotPingEndpoint,
};
use tokio::time::{sleep, timeout};

const MTU: u16 = 512;
const DEVICE_ADDR: ergot::Address = ergot::Address {
    network_id: 1,
    node_id: 2,
    port_id: 0,
};

/// Spawn a ping server on the target stack.
fn spawn_ping_server(stack: &EdgeStack) {
    tokio::spawn({
        let stack = stack.clone();
        async move {
            let server = stack
                .endpoints()
                .bounded_server::<ErgotPingEndpoint, 4>(Some("ping"));
            let server = pin!(server);
            let mut hdl = server.attach();
            loop {
                let _ = hdl
                    .serve(|val: &u32| {
                        let v = *val;
                        async move { v }
                    })
                    .await;
            }
        }
    });
}

/// Send a ping with retries (the first ping establishes the target's net_id).
async fn ping_with_retry(stack: &EdgeStack, val: u32) -> u32 {
    for _ in 0..20 {
        let result = timeout(
            Duration::from_millis(500),
            stack
                .endpoints()
                .request::<ErgotPingEndpoint>(DEVICE_ADDR, &val, None),
        )
        .await;
        match result {
            Ok(Ok(v)) => return v,
            _ => sleep(Duration::from_millis(100)).await,
        }
    }
    panic!("ping failed after retries");
}

#[tokio::test]
async fn two_stacks_ping() {
    let (ctrl_read, tgt_write) = tokio::io::duplex(8192);
    let (tgt_read, ctrl_write) = tokio::io::duplex(8192);

    let ctrl_queue = stream_kit::new_std_queue(4096);
    let ctrl_stack = stream_kit::new_controller_stack(&ctrl_queue, MTU);

    let tgt_queue = stream_kit::new_std_queue(4096);
    let tgt_stack = stream_kit::new_target_stack(&tgt_queue, MTU);

    register_controller_stream(
        ctrl_stack.clone(),
        ctrl_read,
        ctrl_write,
        ctrl_queue,
        None,
        None,
    )
    .await
    .unwrap();

    register_target_stream(
        tgt_stack.clone(),
        tgt_read,
        tgt_write,
        tgt_queue,
        None,
        None,
    )
    .await
    .unwrap();

    spawn_ping_server(&tgt_stack);

    let response = ping_with_retry(&ctrl_stack, 42).await;
    assert_eq!(response, 42);

    // A second ping should also work
    let response = ping_with_retry(&ctrl_stack, 100).await;
    assert_eq!(response, 100);
}

#[tokio::test]
async fn liveness_timeout_on_disconnect() {
    let liveness = LivenessConfig { timeout_ms: 2000 };

    let (ctrl_read, tgt_write) = tokio::io::duplex(8192);
    let (tgt_read, ctrl_write) = tokio::io::duplex(8192);

    let ctrl_queue = stream_kit::new_std_queue(4096);
    let ctrl_stack = stream_kit::new_controller_stack(&ctrl_queue, MTU);

    let tgt_queue = stream_kit::new_std_queue(4096);
    let tgt_stack = stream_kit::new_target_stack(&tgt_queue, MTU);

    let ctrl_notify = Arc::new(WaitQueue::new());

    register_controller_stream(
        ctrl_stack.clone(),
        ctrl_read,
        ctrl_write,
        ctrl_queue,
        Some(liveness.clone()),
        Some(ctrl_notify.clone()),
    )
    .await
    .unwrap();

    register_target_stream(
        tgt_stack.clone(),
        tgt_read,
        tgt_write,
        tgt_queue,
        Some(liveness),
        None,
    )
    .await
    .unwrap();

    spawn_ping_server(&tgt_stack);

    // Do a successful ping to establish Active state
    let response = ping_with_retry(&ctrl_stack, 1).await;
    assert_eq!(response, 1);

    // Verify controller is Active
    let state = ctrl_stack.manage_profile(|im| im.interface_state(()));
    assert!(matches!(state, Some(InterfaceState::Active { .. })));

    // Drop the target stack — target workers stop, controller stops receiving frames
    drop(tgt_stack);

    // Wait for liveness timeout + margin
    sleep(Duration::from_millis(3000)).await;

    // Controller should now be Inactive (liveness timeout, transport still exists)
    // or Down (if the transport error was detected first)
    let state = ctrl_stack.manage_profile(|im| im.interface_state(()));
    assert!(
        matches!(
            state,
            Some(InterfaceState::Inactive) | Some(InterfaceState::Down)
        ),
        "expected Inactive or Down, got {:?}",
        state
    );
}

#[tokio::test]
async fn no_liveness_no_crash_on_disconnect() {
    // Without liveness tracking, peer disconnect shouldn't crash
    let (ctrl_read, tgt_write) = tokio::io::duplex(8192);
    let (tgt_read, ctrl_write) = tokio::io::duplex(8192);

    let ctrl_queue = stream_kit::new_std_queue(4096);
    let ctrl_stack = stream_kit::new_controller_stack(&ctrl_queue, MTU);

    let tgt_queue = stream_kit::new_std_queue(4096);
    let tgt_stack = stream_kit::new_target_stack(&tgt_queue, MTU);

    register_controller_stream(
        ctrl_stack.clone(),
        ctrl_read,
        ctrl_write,
        ctrl_queue,
        None,
        None,
    )
    .await
    .unwrap();

    register_target_stream(
        tgt_stack.clone(),
        tgt_read,
        tgt_write,
        tgt_queue,
        None,
        None,
    )
    .await
    .unwrap();

    spawn_ping_server(&tgt_stack);

    let response = ping_with_retry(&ctrl_stack, 1).await;
    assert_eq!(response, 1);

    // Verify Active
    let state = ctrl_stack.manage_profile(|im| im.interface_state(()));
    assert!(matches!(state, Some(InterfaceState::Active { .. })));

    // Drop target — workers will stop when duplex read returns EOF
    drop(tgt_stack);

    // Wait a bit — shouldn't crash
    sleep(Duration::from_millis(200)).await;
}
