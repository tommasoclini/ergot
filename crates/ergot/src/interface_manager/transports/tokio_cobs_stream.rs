//! Generic tokio COBS stream RxWorker.
//!
//! Works with any [`FrameProcessor`] and any [`AsyncRead`] reader.
//! Used by TCP, generic stream, and serial transports for both
//! `DirectEdge` and `DirectRouter` profiles.
//!
//! [`FrameProcessor`]: crate::interface_manager::FrameProcessor

use std::sync::Arc;

use crate::{
    interface_manager::{
        FrameProcessor, InterfaceState, LivenessConfig, Profile,
        utils::std::{
            ReceiverError,
            acc::{CobsAccumulator, FeedResult},
        },
    },
    logging::warn,
    net_stack::NetStackHandle,
};
use maitake_sync::WaitQueue;
use tokio::{io::AsyncReadExt, select};

/// A generic COBS stream RxWorker for tokio-based transports.
///
/// Reads from an [`AsyncRead`] source, decodes COBS frames, and feeds
/// them to a [`FrameProcessor`]. Supports optional liveness timeout
/// and state change notifications.
///
/// The caller is responsible for:
/// - Setting initial interface state before calling [`run`](Self::run)
/// - Cleaning up on exit (set Down, deregister, close closer, notify)
pub struct CobsStreamRxWorker<N, R, P>
where
    N: NetStackHandle,
    R: AsyncReadExt + Unpin,
    P: FrameProcessor<N>,
{
    pub(crate) nsh: N,
    pub(crate) reader: R,
    pub(crate) closer: Arc<WaitQueue>,
    pub(crate) processor: P,
    pub(crate) ident: <<N as NetStackHandle>::Profile as Profile>::InterfaceIdent,
    pub(crate) liveness: Option<LivenessConfig>,
    pub(crate) state_notify: Option<Arc<WaitQueue>>,
    pub(crate) cobs_buf_size: usize,
}

impl<N, R, P> CobsStreamRxWorker<N, R, P>
where
    N: NetStackHandle,
    R: AsyncReadExt + Unpin,
    P: FrameProcessor<N>,
{
    fn notify(&self) {
        if let Some(notify) = &self.state_notify {
            notify.wake_all();
        }
    }

    /// Run the receive loop until the connection is lost or closer fires.
    ///
    /// Returns `ReceiverError` indicating the reason for exit. The caller
    /// should handle cleanup (state transitions, deregistration, etc.).
    pub async fn run(&mut self) -> ReceiverError {
        let mut cobs_buf = CobsAccumulator::new(self.cobs_buf_size);
        let mut raw_buf = vec![0u8; 4096].into_boxed_slice();
        let mut have_received = false;

        loop {
            let ct = match self
                .read_or_timeout(&mut raw_buf, &mut have_received, &mut cobs_buf)
                .await
            {
                Ok(ct) => ct,
                Err(e) => return e,
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
                        let changed =
                            self.processor
                                .process_frame(data, &self.nsh, self.ident.clone());
                        have_received = true;
                        if changed {
                            self.notify();
                        }
                        remaining
                    }
                };
            }
        }
    }

    async fn read_or_timeout(
        &mut self,
        buf: &mut [u8],
        have_received: &mut bool,
        cobs_buf: &mut CobsAccumulator,
    ) -> Result<usize, ReceiverError> {
        loop {
            let rd = self.reader.read(buf);
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
                        let changed = self.nsh.stack().manage_profile(|im| {
                            if matches!(
                                im.interface_state(self.ident.clone()),
                                Some(InterfaceState::Active { .. })
                            ) {
                                _ = im.set_interface_state(
                                    self.ident.clone(),
                                    InterfaceState::Inactive,
                                );
                                true
                            } else {
                                false
                            }
                        });
                        if changed {
                            warn!("Liveness timeout — interface inactive");
                            self.notify();
                        }
                        self.processor.reset();
                        *have_received = false;
                        *cobs_buf = CobsAccumulator::new(self.cobs_buf_size);
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
