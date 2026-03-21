//! A generic tokio stream edge device profile
//!
//! This is useful for std based devices/applications that can directly connect to a DirectRouter
//! using any tokio AsyncRead/AsyncWrite stream (TCP, serial, RTT adapters, etc.).

use std::sync::Arc;

use crate::{
    interface_manager::{
        InterfaceState, Profile,
        interface_impls::tokio_stream::TokioStreamInterface,
        profiles::direct_edge::{CENTRAL_NODE_ID, DirectEdge, process_frame},
        utils::std::{
            ReceiverError, StdQueue,
            acc::{CobsAccumulator, FeedResult},
        },
    },
    net_stack::NetStackHandle,
};

use crate::logging::{error, info, trace, warn};
use bbqueue::{prod_cons::stream::StreamConsumer, traits::bbqhdl::BbqHandle};
use maitake_sync::WaitQueue;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    select,
};

pub struct RxWorker<N: NetStackHandle> {
    stack: N,
    reader: Box<dyn AsyncRead + Unpin + Send>,
    closer: Arc<WaitQueue>,
    initial_net_id: Option<u16>,
}

// ---- impls ----

impl<N> RxWorker<N>
where
    N: NetStackHandle<Profile = DirectEdge<TokioStreamInterface>>,
{
    pub async fn run(mut self) -> Result<(), ReceiverError> {
        let res = self.run_inner().await;
        self.stack.stack().manage_profile(|im| {
            _ = im.set_interface_state((), InterfaceState::Down);
        });
        res
    }

    pub async fn run_inner(&mut self) -> Result<(), ReceiverError> {
        let mut cobs_buf = CobsAccumulator::new(1024 * 1024);
        let mut raw_buf = [0u8; 4096];
        let mut net_id = self.initial_net_id;

        loop {
            let rd = self.reader.read(&mut raw_buf);
            let close = self.closer.wait();

            let ct = select! {
                r = rd => {
                    match r {
                        Ok(0) | Err(_) => {
                            warn!("recv run closed");
                            return Err(ReceiverError::SocketClosed)
                        },
                        Ok(ct) => ct,
                    }
                }
                _c = close => {
                    return Err(ReceiverError::SocketClosed);
                }
            };

            let buf = &mut raw_buf[..ct];
            let mut window = buf;

            'cobs: while !window.is_empty() {
                window = match cobs_buf.feed_raw(window) {
                    FeedResult::Consumed => break 'cobs,
                    FeedResult::OverFull(new_wind) => new_wind,
                    FeedResult::DecodeError(new_wind) => new_wind,
                    FeedResult::Success { data, remaining }
                    | FeedResult::SuccessInput { data, remaining } => {
                        process_frame(&mut net_id, data, &self.stack, ());
                        remaining
                    }
                };
            }
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct SocketAlreadyActive;

// Helper functions

pub async fn register_target_stream<N>(
    stack: N,
    reader: impl AsyncRead + Unpin + Send + 'static,
    writer: impl AsyncWrite + Unpin + Send + 'static,
    queue: StdQueue,
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

        im.set_interface_state((), InterfaceState::Inactive)
            .map_err(|_| SocketAlreadyActive)?;

        Ok(())
    })?;
    let rx_worker = RxWorker {
        stack,
        reader: Box::new(reader),
        closer: closer.clone(),
        initial_net_id: None,
    };
    tokio::task::spawn(tx_worker(
        Box::new(writer),
        queue.stream_consumer(),
        closer.clone(),
    ));
    tokio::task::spawn(rx_worker.run());
    Ok(())
}

pub async fn register_controller_stream<N>(
    stack: N,
    reader: impl AsyncRead + Unpin + Send + 'static,
    writer: impl AsyncWrite + Unpin + Send + 'static,
    queue: StdQueue,
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
    let rx_worker = RxWorker {
        stack,
        reader: Box::new(reader),
        closer: closer.clone(),
        initial_net_id: Some(1),
    };
    tokio::task::spawn(tx_worker(
        Box::new(writer),
        queue.stream_consumer(),
        closer.clone(),
    ));
    tokio::task::spawn(rx_worker.run());
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
