//! A UDP based DirectRouter

use crate::logging::{debug, error, info, warn};
use bbqueue::{prod_cons::framed::FramedConsumer, traits::bbqhdl::BbqHandle};
use maitake_sync::WaitQueue;
use std::sync::Arc;
use tokio::{net::UdpSocket, select};

use crate::{
    interface_manager::{
        InterfaceState, LivenessConfig, Profile,
        interface_impls::tokio_udp::TokioUdpInterface,
        profiles::direct_router::{DirectRouter, RouterFrameProcessor},
        transports::tokio_udp::UdpRxWorker,
        utils::{
            framed_stream::Sink,
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
    tx: Arc<UdpSocket>,
    rx: FramedConsumer<StdQueue>,
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
            let res = self.tx.send(&frame).await;
            frame.release();
            if let Err(e) = res {
                error!("Tx Error. socket: {:?}, error: {:?}", self.tx, e);
                break;
            }
        }
    }
}

pub async fn register_interface<N>(
    stack: N,
    socket: UdpSocket,
    max_ergot_packet_size: u16,
    outgoing_buffer_size: usize,
    liveness: Option<LivenessConfig>,
    state_notify: Option<Arc<WaitQueue>>,
) -> Result<u64, Error>
where
    N: NetStackHandle<Profile = DirectRouter<TokioUdpInterface>>,
    N: Send + 'static,
{
    let arc_socket = Arc::new(socket);
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

    let notify_clone = state_notify.clone();
    let nsh_clone = stack.clone();

    let mut rx_worker = UdpRxWorker {
        nsh: stack.clone(),
        skt: arc_socket.clone(),
        closer: closer.clone(),
        processor: RouterFrameProcessor::new(net_id),
        ident,
        liveness,
        state_notify,
    };

    let tx_worker = TxWorker {
        net_id,
        tx: arc_socket,
        rx: <StdQueue as BbqHandle>::framed_consumer(&q),
        closer: closer.clone(),
    };

    stack.stack().manage_profile(|im| {
        im.set_interface_closer(ident, closer.clone());
    });

    tokio::task::spawn(async move {
        let close = rx_worker.closer.clone();
        select! {
            _run = rx_worker.run(|_addr| {}) => {
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
