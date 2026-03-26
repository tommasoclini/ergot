//! Generic tokio UDP RxWorker.
//!
//! Works with any [`FrameProcessor`] and a shared [`UdpSocket`].
//! UDP datagrams are treated as complete frames (no COBS encoding).
//!
//! [`FrameProcessor`]: crate::interface_manager::FrameProcessor

use std::net::SocketAddr;
use std::sync::Arc;

use crate::{
    interface_manager::{
        FrameProcessor, InterfaceState, LivenessConfig, Profile, utils::std::ReceiverError,
    },
    logging::{trace, warn},
    net_stack::NetStackHandle,
};
use maitake_sync::WaitQueue;
use tokio::{net::UdpSocket, select};

/// A generic UDP datagram RxWorker for tokio-based transports.
///
/// Each received UDP datagram is treated as a complete frame and
/// passed to the [`FrameProcessor`].
///
/// On liveness timeout, transitions to [`InterfaceState::Down`]
/// (not Inactive) because UDP is connectionless — there is no
/// persistent connection to recover.
///
/// The caller is responsible for cleanup after [`run`](Self::run)
/// returns.
pub struct UdpRxWorker<N, P>
where
    N: NetStackHandle,
    P: FrameProcessor<N>,
{
    pub(crate) nsh: N,
    pub(crate) skt: Arc<UdpSocket>,
    pub(crate) closer: Arc<WaitQueue>,
    pub(crate) processor: P,
    pub(crate) ident: <<N as NetStackHandle>::Profile as Profile>::InterfaceIdent,
    pub(crate) liveness: Option<LivenessConfig>,
    pub(crate) state_notify: Option<Arc<WaitQueue>>,
}

/// Result of receiving a UDP datagram.
pub struct RecvResult {
    /// Number of bytes received.
    pub len: usize,
    /// Source address of the datagram.
    pub addr: SocketAddr,
}

impl<N, P> UdpRxWorker<N, P>
where
    N: NetStackHandle,
    P: FrameProcessor<N>,
{
    fn notify(&self) {
        if let Some(notify) = &self.state_notify {
            notify.wake_all();
        }
    }

    /// Run the receive loop.
    ///
    /// Returns each received datagram's source address via the callback
    /// `on_recv`, allowing callers to implement peer discovery.
    /// Returns `ReceiverError` when the connection is lost.
    pub async fn run(&mut self, mut on_recv: impl FnMut(SocketAddr)) -> ReceiverError {
        let mut raw_buf = vec![0u8; 4096].into_boxed_slice();
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
                                warn!("receiver error, retrying. error: {}, kind: {}", e, e.kind());
                                continue;
                            }
                            Ok((ct, addr)) => {
                                trace!("received {} bytes from {}", ct, addr);
                                (ct, addr)
                            }
                        }
                    }
                    _c = close => {
                        return ReceiverError::SocketClosed;
                    }
                    _ = timeout => {
                        warn!("Liveness timeout — interface down");
                        self.nsh.stack().manage_profile(|im| {
                            _ = im.set_interface_state(
                                self.ident.clone(),
                                InterfaceState::Down,
                            );
                        });
                        self.notify();
                        return ReceiverError::SocketClosed;
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
                                warn!("receiver error, retrying. error: {}, kind: {}", e, e.kind());
                                continue;
                            }
                            Ok((ct, addr)) => {
                                trace!("received {} bytes from {}", ct, addr);
                                (ct, addr)
                            }
                        }
                    }
                    _c = close => {
                        return ReceiverError::SocketClosed;
                    }
                }
            };

            have_received = true;
            on_recv(remote_addr);

            let buf = &mut raw_buf[..ct];
            let changed = self
                .processor
                .process_frame(buf, &self.nsh, self.ident.clone());
            if changed {
                self.notify();
            }
        }
    }
}
