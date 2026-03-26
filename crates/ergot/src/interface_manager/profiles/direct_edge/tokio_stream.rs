//! A generic async stream "Edge" device profile
//!
//! This handles any `AsyncRead + AsyncWrite` transport (TCP, serial, etc.)
//! as a single-interface edge device.

use std::sync::Arc;

use crate::{
    interface_manager::{
        InterfaceState, LivenessConfig, Profile,
        interface_impls::tokio_stream::TokioStreamInterface,
        profiles::direct_edge::{CENTRAL_NODE_ID, DirectEdge, EdgeFrameProcessor},
        transports::tokio_cobs_stream::CobsStreamRxWorker,
        utils::std::StdQueue,
    },
    logging::{error, info, trace, warn},
    net_stack::NetStackHandle,
};

use bbqueue::{prod_cons::stream::StreamConsumer, traits::bbqhdl::BbqHandle};
use maitake_sync::WaitQueue;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    select,
};

#[derive(Debug, PartialEq)]
pub struct SocketAlreadyActive;

pub async fn register_target_stream<N>(
    stack: N,
    reader: impl AsyncRead + Unpin + Send + 'static,
    writer: impl AsyncWrite + Unpin + Send + 'static,
    queue: StdQueue,
    liveness: Option<LivenessConfig>,
    state_notify: Option<Arc<WaitQueue>>,
) -> Result<(), SocketAlreadyActive>
where
    N: NetStackHandle<Profile = DirectEdge<TokioStreamInterface>>,
    N: Send + 'static,
{
    let closer = Arc::new(WaitQueue::new());
    stack.stack().manage_profile(|im| {
        match im.interface_state(()) {
            Some(InterfaceState::Down) => {}
            Some(InterfaceState::Inactive) => return Err(SocketAlreadyActive),
            Some(InterfaceState::ActiveLocal { .. }) => return Err(SocketAlreadyActive),
            Some(InterfaceState::Active { .. }) => return Err(SocketAlreadyActive),
            None => {}
        }

        im.set_closer(closer.clone());

        im.set_interface_state((), InterfaceState::Inactive)
            .map_err(|_| SocketAlreadyActive)?;

        Ok(())
    })?;
    if let Some(notify) = &state_notify {
        notify.wake_all();
    }

    let notify_clone = state_notify.clone();
    let stack_clone = stack.clone();

    let mut rx_worker = CobsStreamRxWorker {
        nsh: stack,
        reader: Box::new(reader) as Box<dyn AsyncRead + Unpin + Send>,
        closer: closer.clone(),
        processor: EdgeFrameProcessor::new(),
        ident: (),
        liveness,
        state_notify,
        cobs_buf_size: 1024 * 1024,
    };

    tokio::task::spawn(tx_worker(
        Box::new(writer),
        queue.stream_consumer(),
        closer.clone(),
    ));
    tokio::task::spawn(async move {
        let _res = rx_worker.run().await;
        stack_clone.stack().manage_profile(|im| {
            _ = im.set_interface_state((), InterfaceState::Down);
        });
        if let Some(notify) = &notify_clone {
            notify.wake_all();
        }
    });
    Ok(())
}

pub async fn register_controller_stream<N>(
    stack: N,
    reader: impl AsyncRead + Unpin + Send + 'static,
    writer: impl AsyncWrite + Unpin + Send + 'static,
    queue: StdQueue,
    liveness: Option<LivenessConfig>,
    state_notify: Option<Arc<WaitQueue>>,
) -> Result<(), SocketAlreadyActive>
where
    N: NetStackHandle<Profile = DirectEdge<TokioStreamInterface>>,
    N: Send + 'static,
{
    let closer = Arc::new(WaitQueue::new());
    stack.stack().manage_profile(|im| {
        match im.interface_state(()) {
            Some(InterfaceState::Down) => {}
            Some(InterfaceState::Inactive) => return Err(SocketAlreadyActive),
            Some(InterfaceState::ActiveLocal { .. }) => return Err(SocketAlreadyActive),
            Some(InterfaceState::Active { .. }) => return Err(SocketAlreadyActive),
            None => {}
        }

        im.set_closer(closer.clone());

        im.set_interface_state(
            (),
            InterfaceState::Active {
                net_id: 1,
                node_id: CENTRAL_NODE_ID,
            },
        )
        .map_err(|_| SocketAlreadyActive)?;

        Ok(())
    })?;
    if let Some(notify) = &state_notify {
        notify.wake_all();
    }

    let notify_clone = state_notify.clone();
    let stack_clone = stack.clone();

    let mut rx_worker = CobsStreamRxWorker {
        nsh: stack,
        reader: Box::new(reader) as Box<dyn AsyncRead + Unpin + Send>,
        closer: closer.clone(),
        processor: EdgeFrameProcessor::new_controller(1),
        ident: (),
        liveness,
        state_notify,
        cobs_buf_size: 1024 * 1024,
    };

    tokio::task::spawn(tx_worker(
        Box::new(writer),
        queue.stream_consumer(),
        closer.clone(),
    ));
    tokio::task::spawn(async move {
        let _res = rx_worker.run().await;
        stack_clone.stack().manage_profile(|im| {
            _ = im.set_interface_state((), InterfaceState::Down);
        });
        if let Some(notify) = &notify_clone {
            notify.wake_all();
        }
    });
    Ok(())
}

async fn tx_worker(
    mut tx: Box<dyn AsyncWrite + Unpin + Send>,
    rx: StreamConsumer<StdQueue>,
    closer: Arc<WaitQueue>,
) {
    info!("Started tx_worker");
    loop {
        let rxf = rx.wait_read();
        let clf = closer.wait();

        let frame = select! {
            r = rxf => r,
            _c = clf => {
                break;
            }
        };

        let len = frame.len();
        trace!("sending pkt len:{}", len);
        let res = tx.write_all(&frame).await;
        frame.release(len);
        if let Err(e) = res {
            error!("Tx Error. error: {:?}", e);
            break;
        }
    }
    warn!("Closing interface");
}
