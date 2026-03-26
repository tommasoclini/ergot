//! A generic async stream DirectRouter
//!
//! This implementation can be used to connect to a number of direct edge devices
//! via any `AsyncRead + AsyncWrite` transport (TCP, serial, etc.).

use crate::logging::{debug, error, info, warn};
use bbqueue::{prod_cons::stream::StreamConsumer, traits::bbqhdl::BbqHandle};
use cobs::max_encoding_overhead;
use maitake_sync::WaitQueue;
use std::sync::Arc;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    select,
};

use crate::{
    interface_manager::{
        InterfaceState, LivenessConfig, Profile,
        interface_impls::tokio_stream::TokioStreamInterface,
        profiles::direct_router::{DirectRouter, RouterFrameProcessor},
        transports::tokio_cobs_stream::CobsStreamRxWorker,
        utils::{
            cobs_stream::Sink,
            std::{StdQueue, new_std_queue},
        },
    },
    net_stack::NetStackHandle,
};

#[derive(Debug, PartialEq)]
pub enum Error {
    OutOfNetIds,
}

struct TxWorker {
    net_id: u16,
    tx: Box<dyn AsyncWrite + Unpin + Send>,
    rx: StreamConsumer<StdQueue>,
    closer: Arc<WaitQueue>,
}

impl TxWorker {
    async fn run(mut self) {
        self.run_inner().await;
        warn!("Closing interface {}", self.net_id);
        self.closer.close();
    }

    async fn run_inner(&mut self) {
        info!("Started tx_worker for net_id {}", self.net_id);
        loop {
            let rxf = self.rx.wait_read();
            let clf = self.closer.wait();

            let frame = select! {
                r = rxf => r,
                _c = clf => {
                    break;
                }
            };

            let len = frame.len();
            debug!("sending pkt len:{} on net_id {}", len, self.net_id);
            let res = self.tx.write_all(&frame).await;
            frame.release(len);
            if let Err(e) = res {
                error!("Tx Error. error: {:?}", e);
                break;
            }
        }
    }
}

pub async fn register_interface<N>(
    stack: N,
    reader: impl AsyncRead + Unpin + Send + 'static,
    writer: impl AsyncWrite + Unpin + Send + 'static,
    max_ergot_packet_size: u16,
    outgoing_buffer_size: usize,
    liveness: Option<LivenessConfig>,
    state_notify: Option<Arc<WaitQueue>>,
) -> Result<u64, Error>
where
    N: NetStackHandle<Profile = DirectRouter<TokioStreamInterface>>,
    N: Send + 'static,
{
    let q: StdQueue = new_std_queue(outgoing_buffer_size);
    let res = stack.stack().manage_profile(|im| {
        let ident =
            im.register_interface(Sink::new_from_handle(q.clone(), max_ergot_packet_size))?;
        let state = im.interface_state(ident)?;
        match state {
            InterfaceState::Active { net_id, node_id: _ } => Some((ident, net_id)),
            _ => {
                _ = im.deregister_interface(ident);
                None
            }
        }
    });
    let Some((ident, net_id)) = res else {
        return Err(Error::OutOfNetIds);
    };
    let closer = Arc::new(WaitQueue::new());

    let overhead = max_encoding_overhead(max_ergot_packet_size as usize);
    let cobs_buf_size = max_ergot_packet_size as usize + overhead;

    let notify_clone = state_notify.clone();
    let nsh_clone = stack.clone();

    let mut rx_worker = CobsStreamRxWorker {
        nsh: stack.clone(),
        reader: Box::new(reader) as Box<dyn AsyncRead + Unpin + Send>,
        closer: closer.clone(),
        processor: RouterFrameProcessor::new(net_id),
        ident,
        liveness,
        state_notify,
        cobs_buf_size,
    };

    let tx_worker = TxWorker {
        net_id,
        tx: Box::new(writer),
        rx: <StdQueue as BbqHandle>::stream_consumer(&q),
        closer: closer.clone(),
    };

    stack.stack().manage_profile(|im| {
        im.set_interface_closer(ident, closer.clone());
    });

    tokio::task::spawn(async move {
        let close = rx_worker.closer.clone();
        select! {
            _run = rx_worker.run() => {
                close.close();
            },
            _clf = close.wait() => {},
        }
        nsh_clone.stack().manage_profile(|im| {
            _ = im.deregister_interface(ident);
        });
        if let Some(notify) = &notify_clone {
            notify.wake_all();
        }
    });
    tokio::task::spawn(tx_worker.run());

    Ok(ident)
}
