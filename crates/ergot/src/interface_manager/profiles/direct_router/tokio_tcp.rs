//! A TCP based DirectRouter
//!
//! This implementation can be used to connect to a number of direct edge TCP devices.

use crate::logging::{debug, error, info, warn};
use bbqueue::{prod_cons::stream::StreamConsumer, traits::bbqhdl::BbqHandle};
use maitake_sync::WaitQueue;
use std::sync::Arc;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpStream, tcp::OwnedWriteHalf},
    select,
};

use crate::{
    interface_manager::{
        InterfaceState, LivenessConfig, Profile,
        interface_impls::tokio_tcp::TokioTcpInterface,
        profiles::direct_router::{DirectRouter, RouterFrameProcessor},
        transports::tokio_cobs_stream::CobsStreamRxWorker,
        utils::{
            cobs_stream::Sink,
            std::{StdQueue, new_std_queue},
        },
    },
    net_stack::NetStackHandle,
};

use cobs::max_encoding_overhead;

#[derive(Debug, PartialEq)]
pub enum Error {
    OutOfNetIds,
}

struct TxWorker {
    net_id: u16,
    tx: OwnedWriteHalf,
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
                error!("Tx Error. socket: {:?}, error: {:?}", self.tx, e);
                break;
            }
        }
    }
}

pub async fn register_interface<N>(
    stack: N,
    socket: TcpStream,
    max_ergot_packet_size: u16,
    outgoing_buffer_size: usize,
    liveness: Option<LivenessConfig>,
    state_notify: Option<Arc<WaitQueue>>,
) -> Result<u64, Error>
where
    N: NetStackHandle<Profile = DirectRouter<TokioTcpInterface>>,
    N: Send + 'static,
{
    let (rx, tx) = socket.into_split();
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
    let closer_clone = closer.clone();
    let nsh_clone = stack.clone();

    let mut rx_worker = CobsStreamRxWorker {
        nsh: stack.clone(),
        reader: rx,
        closer: closer.clone(),
        processor: RouterFrameProcessor::new(net_id),
        ident,
        liveness,
        state_notify,
        cobs_buf_size,
    };

    // Store closer so deregister_interface() can signal workers
    stack.stack().manage_profile(|im| {
        im.set_interface_closer(ident, closer.clone());
    });

    let tx_worker = TxWorker {
        net_id,
        tx,
        rx: <StdQueue as BbqHandle>::stream_consumer(&q),
        closer: closer_clone,
    };

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
