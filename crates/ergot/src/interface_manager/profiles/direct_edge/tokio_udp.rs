//! A std+udp edge device profile
//!
//! Targets can use unconnected sockets (bind only, no connect) — the peer
//! address is learned from the first received packet. Controllers use
//! connected sockets.

use std::net::SocketAddr;
use std::sync::Arc;

use crate::{
    interface_manager::{
        InterfaceState, LivenessConfig, Profile,
        interface_impls::tokio_udp::TokioUdpInterface,
        profiles::direct_edge::{CENTRAL_NODE_ID, DirectEdge, EdgeFrameProcessor},
        transports::tokio_udp::UdpRxWorker,
        utils::std::StdQueue,
    },
    logging::{error, info, trace, warn},
    net_stack::NetStackHandle,
};

use bbqueue::prod_cons::framed::FramedConsumer;
use bbqueue::traits::bbqhdl::BbqHandle;
use maitake_sync::WaitQueue;
use tokio::{net::UdpSocket, select, sync::watch};

pub type StdUdpClientIm = DirectEdge<TokioUdpInterface>;

#[derive(Debug, PartialEq)]
pub struct SocketAlreadyActive;

pub enum InterfaceKind {
    Target,
    Controller,
}

pub async fn register_interface<N>(
    stack: N,
    socket: UdpSocket,
    queue: StdQueue,
    interface_kind: InterfaceKind,
    _ident: <<N as NetStackHandle>::Profile as Profile>::InterfaceIdent,
    liveness: Option<LivenessConfig>,
    state_notify: Option<Arc<WaitQueue>>,
) -> Result<(), SocketAlreadyActive>
where
    N: NetStackHandle<Profile = DirectEdge<TokioUdpInterface>>,
    N: Send + 'static,
{
    let arc_socket = Arc::new(socket);

    let closer = Arc::new(WaitQueue::new());
    let (initial_net_id, peer_tx) = stack.stack().manage_profile(|im| {
        match im.interface_state(()) {
            Some(InterfaceState::Down) => {}
            Some(InterfaceState::Inactive) => return Err(SocketAlreadyActive),
            Some(InterfaceState::ActiveLocal { .. }) => return Err(SocketAlreadyActive),
            Some(InterfaceState::Active { .. }) => return Err(SocketAlreadyActive),
            None => {}
        }

        im.set_closer(closer.clone());

        match interface_kind {
            InterfaceKind::Controller => {
                trace!("UDP controller is active");
                im.set_interface_state(
                    (),
                    InterfaceState::Active {
                        net_id: 1,
                        node_id: CENTRAL_NODE_ID,
                    },
                )
                .map_err(|_| SocketAlreadyActive)?;
                Ok((Some(1u16), None))
            }
            InterfaceKind::Target => {
                trace!("UDP target is inactive, waiting for first frame");
                im.set_interface_state((), InterfaceState::Inactive)
                    .map_err(|_| SocketAlreadyActive)?;
                let (tx, rx) = watch::channel(None);
                Ok((None, Some((tx, rx))))
            }
        }
    })?;

    if let Some(notify) = &state_notify {
        notify.wake_all();
    }

    let (peer_sender, peer_receiver) = match peer_tx {
        Some((tx, rx)) => (Some(tx), Some(rx)),
        None => (None, None),
    };

    let processor = match initial_net_id {
        Some(net_id) => EdgeFrameProcessor::new_controller(net_id),
        None => EdgeFrameProcessor::new(),
    };

    let notify_clone = state_notify.clone();
    let stack_clone = stack.clone();

    let mut rx_worker = UdpRxWorker {
        nsh: stack,
        skt: arc_socket.clone(),
        closer: closer.clone(),
        processor,
        ident: (),
        liveness,
        state_notify,
    };

    tokio::task::spawn(async move {
        let _res = rx_worker
            .run(|addr| {
                if let Some(peer_tx) = &peer_sender {
                    let _ = peer_tx.send(Some(addr));
                }
            })
            .await;
        stack_clone.stack().manage_profile(|im| {
            _ = im.set_interface_state((), InterfaceState::Down);
        });
        if let Some(notify) = &notify_clone {
            notify.wake_all();
        }
    });
    tokio::task::spawn(tx_worker(
        arc_socket,
        queue.framed_consumer(),
        closer.clone(),
        peer_receiver,
    ));
    Ok(())
}

async fn tx_worker(
    tx: Arc<UdpSocket>,
    rx: FramedConsumer<StdQueue>,
    closer: Arc<WaitQueue>,
    peer_rx: Option<watch::Receiver<Option<SocketAddr>>>,
) {
    info!("Started tx_worker");

    let peer_addr = if let Some(mut peer_rx) = peer_rx {
        loop {
            let clf = closer.wait();
            let changed = peer_rx.changed();

            select! {
                r = changed => {
                    if r.is_err() {
                        warn!("Peer address channel closed");
                        return;
                    }
                    if let Some(addr) = *peer_rx.borrow() {
                        info!("Learned peer address: {}", addr);
                        break Some(addr);
                    }
                }
                _c = clf => {
                    return;
                }
            }
        }
    } else {
        None
    };

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
        let res = match peer_addr {
            Some(addr) => tx.send_to(&frame, addr).await,
            None => tx.send(&frame).await,
        };
        frame.release();
        if let Err(e) = res {
            error!("Tx Error. error: {:?}", e);
        }
    }
    warn!("Closing interface");
}
