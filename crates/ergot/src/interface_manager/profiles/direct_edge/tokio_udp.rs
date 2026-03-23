//! A std+udp edge device profile
//!
//! This is useful for std based devices/applications that can directly connect to a DirectRouter
//! using a udp connection.
//!
//! Targets can use unconnected sockets (bind only, no connect) — the peer address
//! is learned from the first received packet. Controllers use connected sockets.

use std::net::SocketAddr;
use std::sync::Arc;

use crate::{
    interface_manager::{
        InterfaceState, LivenessConfig, Profile,
        interface_impls::tokio_udp::TokioUdpInterface,
        profiles::direct_edge::{DirectEdge, process_frame},
        utils::std::{ReceiverError, StdQueue},
    },
    net_stack::NetStackHandle,
};

use crate::interface_manager::profiles::direct_edge::CENTRAL_NODE_ID;
use crate::logging::{error, info, trace, warn};
use bbqueue::prod_cons::framed::FramedConsumer;
use bbqueue::traits::bbqhdl::BbqHandle;
use maitake_sync::WaitQueue;
use tokio::{net::UdpSocket, select, sync::watch};

pub type StdUdpClientIm = DirectEdge<TokioUdpInterface>;

pub struct RxWorker<N: NetStackHandle> {
    stack: N,
    skt: Arc<UdpSocket>,
    closer: Arc<WaitQueue>,
    initial_net_id: Option<u16>,
    liveness: Option<LivenessConfig>,
    state_notify: Option<Arc<WaitQueue>>,
    /// Sends learned peer address to TxWorker (for unconnected target sockets)
    peer_tx: Option<watch::Sender<Option<SocketAddr>>>,
}

// ---- impls ----

impl<N> RxWorker<N>
where
    N: NetStackHandle<Profile = DirectEdge<TokioUdpInterface>>,
{
    pub async fn run(mut self) -> Result<(), ReceiverError> {
        info!("Started rx_worker");

        let res = self.run_inner().await;

        info!("Finished rx_worker");

        self.stack.stack().manage_profile(|im| {
            _ = im.set_interface_state((), InterfaceState::Down);
        });
        if let Some(notify) = &self.state_notify {
            notify.wake_all();
        }
        res
    }

    pub async fn run_inner(&mut self) -> Result<(), ReceiverError> {
        let mut raw_buf = [0u8; 4096];
        let mut net_id = self.initial_net_id;
        let mut have_received = false;

        loop {
            let rd = self.skt.recv_from(&mut raw_buf);
            let close = self.closer.wait();

            let liveness_active = self.liveness.is_some() && have_received;

            let (ct, remote_addr) = if liveness_active {
                let liveness = self.liveness.as_ref().unwrap();
                let timeout =
                    tokio::time::sleep(tokio::time::Duration::from_millis(liveness.timeout_ms));

                select! {
                    r = rd => {
                        match r {
                            Ok((0, _)) => {
                                warn!("received nothing, retrying");
                                continue;
                            }
                            Err(e) => {
                                warn!("receiver error, retrying. error: {}", e);
                                continue;
                            }
                            Ok((ct, addr)) => {
                                trace!("received {} bytes from {}", ct, addr);
                                (ct, addr)
                            }
                        }
                    }
                    _c = close => {
                        return Err(ReceiverError::SocketClosed);
                    }
                    _ = timeout => {
                        // UDP is connectionless — no persistent connection to recover.
                        // Transition to Down so workers exit and the socket is clean
                        // for the next host.
                        self.stack.stack().manage_profile(|im| {
                            _ = im.set_interface_state((), InterfaceState::Down);
                        });
                        warn!("Liveness timeout — interface down");
                        if let Some(notify) = &self.state_notify {
                            notify.wake_all();
                        }
                        return Err(ReceiverError::SocketClosed);
                    }
                }
            } else {
                select! {
                    r = rd => {
                        match r {
                            Ok((0, _)) => {
                                warn!("received nothing, retrying");
                                continue;
                            }
                            Err(e) => {
                                warn!("receiver error, retrying. error: {}", e);
                                continue;
                            }
                            Ok((ct, addr)) => {
                                trace!("received {} bytes from {}", ct, addr);
                                (ct, addr)
                            }
                        }
                    }
                    _c = close => {
                        return Err(ReceiverError::SocketClosed);
                    }
                }
            };

            have_received = true;

            // Update peer address for TxWorker (unconnected target sockets)
            if let Some(peer_tx) = &self.peer_tx {
                let _ = peer_tx.send(Some(remote_addr));
            }

            let prev_net_id = net_id;

            let buf = &mut raw_buf[..ct];
            process_frame(&mut net_id, buf, &self.stack, ());

            // If net_id changed (state transitioned to Active), notify
            if net_id != prev_net_id
                && let Some(notify) = &self.state_notify
            {
                notify.wake_all();
            }
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct SocketAlreadyActive;

// Helper functions

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
                // Controller uses connected socket — no peer discovery needed
                Ok((Some(1u16), None))
            }
            InterfaceKind::Target => {
                trace!("UDP target is inactive, waiting for first frame");
                im.set_interface_state((), InterfaceState::Inactive)
                    .map_err(|_| SocketAlreadyActive)?;
                // Target uses unconnected socket — peer address learned from first recv_from
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

    let rx_worker = RxWorker {
        stack,
        skt: arc_socket.clone(),
        closer: closer.clone(),
        initial_net_id,
        liveness,
        state_notify,
        peer_tx: peer_sender,
    };
    tokio::task::spawn(tx_worker(
        arc_socket,
        queue.framed_consumer(),
        closer.clone(),
        peer_receiver,
    ));
    tokio::task::spawn(rx_worker.run());
    Ok(())
}

async fn tx_worker(
    tx: Arc<UdpSocket>,
    rx: FramedConsumer<StdQueue>,
    closer: Arc<WaitQueue>,
    peer_rx: Option<watch::Receiver<Option<SocketAddr>>>,
) {
    info!("Started tx_worker");

    // For unconnected sockets (target), wait for peer address from RxWorker
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
        None // Connected socket (controller) — use send()
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
