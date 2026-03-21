//! A generic stream based DirectRouter
//!
//! This implementation can be used to connect to direct edge devices over any
//! tokio AsyncRead/AsyncWrite stream (TCP, serial, RTT adapters, etc.).

use crate::logging::{debug, error, info, warn};
use bbqueue::{prod_cons::stream::StreamConsumer, traits::bbqhdl::BbqHandle};
use cobs::max_encoding_overhead;
use maitake_sync::WaitQueue;
use std::sync::Arc;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    select,
};

use crate::{
    interface_manager::{
        InterfaceState, LivenessConfig, Profile,
        interface_impls::tokio_stream::TokioStreamInterface,
        profiles::direct_router::{DirectRouter, process_frame},
        utils::{
            cobs_stream::Sink,
            std::{
                ReceiverError, StdQueue,
                acc::{CobsAccumulator, FeedResult},
                new_std_queue,
            },
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

struct RxWorker<N>
where
    N: NetStackHandle<Profile = DirectRouter<TokioStreamInterface>>,
    N: Send + 'static,
{
    interface_id: u64,
    net_id: u16,
    nsh: N,
    reader: Box<dyn AsyncRead + Unpin + Send>,
    closer: Arc<WaitQueue>,
    mtu: u16,
    liveness: Option<LivenessConfig>,
    state_notify: Option<Arc<WaitQueue>>,
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
                error!("Tx Error: {:?}", e);
                break;
            }
        }
    }
}

impl<N> RxWorker<N>
where
    N: NetStackHandle<Profile = DirectRouter<TokioStreamInterface>>,
    N: Send + 'static,
{
    async fn run(mut self) {
        let close = self.closer.clone();

        select! {
            run = self.run_inner() => {
                self.closer.close();
                error!("Receive Error: {:?}", run);
            },
            _clf = close.wait() => {},
        }

        self.nsh.stack().manage_profile(|im| {
            _ = im.deregister_interface(self.interface_id);
        });
        if let Some(notify) = &self.state_notify {
            notify.wake_all();
        }
    }

    pub async fn run_inner(&mut self) -> ReceiverError {
        let overhead = max_encoding_overhead(self.mtu as usize);
        let mut cobs_buf = CobsAccumulator::new(self.mtu as usize + overhead);
        let mut raw_buf = vec![0u8; 4096].into_boxed_slice();
        let mut have_received = false;

        loop {
            let rd = self.reader.read(&mut raw_buf);
            let close = self.closer.wait();

            let liveness_active = self.liveness.is_some() && have_received;

            let ct = if liveness_active {
                let liveness = self.liveness.as_ref().unwrap();
                let timeout =
                    tokio::time::sleep(tokio::time::Duration::from_millis(liveness.timeout_ms));

                select! {
                    r = rd => {
                        match r {
                            Ok(0) | Err(_) => {
                                warn!("recv run {} closed", self.net_id);
                                return ReceiverError::SocketClosed;
                            }
                            Ok(ct) => ct,
                        }
                    }
                    _c = close => {
                        return ReceiverError::SocketClosed;
                    }
                    _ = timeout => {
                        warn!("Liveness timeout for net_id {}", self.net_id);
                        self.nsh.stack().manage_profile(|im| {
                            _ = im.set_interface_state(
                                self.interface_id,
                                InterfaceState::Inactive,
                            );
                        });
                        if let Some(notify) = &self.state_notify {
                            notify.wake_all();
                        }
                        have_received = false;
                        cobs_buf = CobsAccumulator::new(self.mtu as usize + overhead);
                        continue;
                    }
                }
            } else {
                select! {
                    r = rd => {
                        match r {
                            Ok(0) | Err(_) => {
                                warn!("recv run {} closed", self.net_id);
                                return ReceiverError::SocketClosed;
                            }
                            Ok(ct) => ct,
                        }
                    }
                    _c = close => {
                        return ReceiverError::SocketClosed;
                    }
                }
            };

            let buf = &mut raw_buf[..ct];
            let mut window = buf;
            let mut got_frame = false;

            'cobs: while !window.is_empty() {
                window = match cobs_buf.feed_raw(window) {
                    FeedResult::Consumed => break 'cobs,
                    FeedResult::OverFull(new_wind) => new_wind,
                    FeedResult::DecodeError(new_wind) => new_wind,
                    FeedResult::Success { data, remaining }
                    | FeedResult::SuccessInput { data, remaining } => {
                        process_frame(self.net_id, data, &self.nsh, self.interface_id);
                        got_frame = true;
                        remaining
                    }
                };
            }

            if got_frame {
                // Re-establish Active if we were Inactive after a liveness timeout
                if !have_received {
                    let changed = self.nsh.stack().manage_profile(|im| {
                        if matches!(
                            im.interface_state(self.interface_id),
                            Some(InterfaceState::Inactive)
                        ) {
                            _ = im.set_interface_state(
                                self.interface_id,
                                InterfaceState::Active {
                                    net_id: self.net_id,
                                    node_id: crate::interface_manager::profiles::direct_edge::CENTRAL_NODE_ID,
                                },
                            );
                            true
                        } else {
                            false
                        }
                    });
                    if changed && let Some(notify) = &self.state_notify {
                        notify.wake_all();
                    }
                }
                have_received = true;
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

    let rx_worker = RxWorker {
        nsh: stack.clone(),
        reader: Box::new(reader),
        closer: closer.clone(),
        mtu: max_ergot_packet_size,
        interface_id: ident,
        net_id,
        liveness,
        state_notify,
    };
    let tx_worker = TxWorker {
        net_id,
        tx: Box::new(writer),
        rx: <StdQueue as BbqHandle>::stream_consumer(&q),
        closer: closer.clone(),
    };

    stack.stack().manage_profile(|im| {
        im.set_interface_closer(ident, closer);
    });

    tokio::task::spawn(rx_worker.run());
    tokio::task::spawn(tx_worker.run());

    Ok(ident)
}
