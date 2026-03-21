//! A std+tcp edge device profile
//!
//! This is useful for std based devices/applications that can directly connect to a DirectRouter
//! using a tcp connection.

use std::sync::Arc;

use crate::{
    interface_manager::{
        InterfaceState, LivenessConfig, Profile,
        interface_impls::tokio_tcp::TokioTcpInterface,
        profiles::direct_edge::{DirectEdge, process_frame},
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
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    select,
};

pub type StdTcpClientIm = DirectEdge<TokioTcpInterface>;

pub struct RxWorker<N: NetStackHandle> {
    stack: N,
    skt: OwnedReadHalf,
    closer: Arc<WaitQueue>,
    liveness: Option<LivenessConfig>,
    state_notify: Option<Arc<WaitQueue>>,
}

// ---- impls ----

impl<N> RxWorker<N>
where
    N: NetStackHandle<Profile = DirectEdge<TokioTcpInterface>>,
{
    pub async fn run(mut self) -> Result<(), ReceiverError> {
        let res = self.run_inner().await;
        self.stack.stack().manage_profile(|im| {
            _ = im.set_interface_state((), InterfaceState::Down);
        });
        if let Some(notify) = &self.state_notify {
            notify.wake_all();
        }
        res
    }

    pub async fn run_inner(&mut self) -> Result<(), ReceiverError> {
        let mut cobs_buf = CobsAccumulator::new(1024 * 1024);
        let mut raw_buf = [0u8; 4096];
        let mut net_id = None;
        let mut have_received = false;
        let mut needs_cobs_reset = false;

        loop {
            let ct = self
                .read_or_timeout(
                    &mut raw_buf,
                    &mut net_id,
                    &mut have_received,
                    &mut needs_cobs_reset,
                )
                .await?;

            if needs_cobs_reset {
                cobs_buf = CobsAccumulator::new(1024 * 1024);
                needs_cobs_reset = false;
            }

            let buf = &mut raw_buf[..ct];
            let mut window = buf;

            let prev_net_id = net_id;

            'cobs: while !window.is_empty() {
                window = match cobs_buf.feed_raw(window) {
                    FeedResult::Consumed => break 'cobs,
                    FeedResult::OverFull(new_wind) => new_wind,
                    FeedResult::DecodeError(new_wind) => new_wind,
                    FeedResult::Success { data, remaining }
                    | FeedResult::SuccessInput { data, remaining } => {
                        process_frame(&mut net_id, data, &self.stack, ());
                        have_received = true;
                        remaining
                    }
                };
            }

            if net_id != prev_net_id
                && let Some(notify) = &self.state_notify
            {
                notify.wake_all();
            }
        }
    }

    async fn read_or_timeout(
        &mut self,
        buf: &mut [u8],
        net_id: &mut Option<u16>,
        have_received: &mut bool,
        needs_cobs_reset: &mut bool,
    ) -> Result<usize, ReceiverError> {
        loop {
            let rd = self.skt.read(buf);
            let close = self.closer.wait();

            let liveness_active = self.liveness.is_some() && *have_received;

            let ct = if liveness_active {
                let liveness = self.liveness.as_ref().unwrap();
                let timeout =
                    tokio::time::sleep(tokio::time::Duration::from_millis(liveness.timeout_ms));

                select! {
                    r = rd => {
                        match r {
                            Ok(0) | Err(_) => {
                                warn!("recv run closed");
                                return Err(ReceiverError::SocketClosed);
                            }
                            Ok(ct) => ct,
                        }
                    }
                    _c = close => {
                        return Err(ReceiverError::SocketClosed);
                    }
                    _ = timeout => {
                        let changed = self.stack.stack().manage_profile(|im| {
                            if matches!(im.interface_state(()), Some(InterfaceState::Active { .. })) {
                                _ = im.set_interface_state((), InterfaceState::Inactive);
                                true
                            } else {
                                false
                            }
                        });
                        if changed {
                            warn!("Liveness timeout — interface inactive");
                            if let Some(notify) = &self.state_notify {
                                notify.wake_all();
                            }
                        }
                        *net_id = None;
                        *have_received = false;
                        *needs_cobs_reset = true;
                        continue;
                    }
                }
            } else {
                select! {
                    r = rd => {
                        match r {
                            Ok(0) | Err(_) => {
                                warn!("recv run closed");
                                return Err(ReceiverError::SocketClosed);
                            }
                            Ok(ct) => ct,
                        }
                    }
                    _c = close => {
                        return Err(ReceiverError::SocketClosed);
                    }
                }
            };

            return Ok(ct);
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct SocketAlreadyActive;

// Helper functions

pub async fn register_target_interface<N>(
    stack: N,
    socket: TcpStream,
    queue: StdQueue,
    liveness: Option<LivenessConfig>,
    state_notify: Option<Arc<WaitQueue>>,
) -> Result<(), SocketAlreadyActive>
where
    N: NetStackHandle<Profile = DirectEdge<TokioTcpInterface>>,
    N: Send + 'static,
{
    let (rx, tx) = socket.into_split();
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
    if let Some(notify) = &state_notify {
        notify.wake_all();
    }
    let rx_worker = RxWorker {
        stack,
        skt: rx,
        closer: closer.clone(),
        liveness,
        state_notify,
    };
    tokio::task::spawn(tx_worker(tx, queue.stream_consumer(), closer.clone()));
    tokio::task::spawn(rx_worker.run());
    Ok(())
}

async fn tx_worker(mut tx: OwnedWriteHalf, rx: StreamConsumer<StdQueue>, closer: Arc<WaitQueue>) {
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
            error!("Tx Error. socket: {:?}, error: {:?}", tx, e);
            break;
        }
    }
    warn!("Closing interface");
}
