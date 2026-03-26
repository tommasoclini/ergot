//! Generic nusb USB bulk RxWorker.
//!
//! Works with any [`FrameProcessor`]. USB bulk transfers are
//! treated as complete frames (no COBS encoding). Includes
//! stall recovery logic for USB error conditions.
//!
//! [`FrameProcessor`]: crate::interface_manager::FrameProcessor

use std::sync::Arc;

use crate::{
    interface_manager::{FrameProcessor, Profile, utils::std::ReceiverError},
    logging::{error, info, trace, warn},
    net_stack::NetStackHandle,
};
use maitake_sync::WaitQueue;
use nusb::transfer::{Queue, RequestBuffer, TransferError};
use tokio::select;

/// How many in-flight requests at once — allows nusb to keep pulling
/// frames even if we haven't processed them host-side yet.
pub const IN_FLIGHT_REQS: usize = 4;

/// How many consecutive IN errors will we try to recover from before
/// giving up?
pub const MAX_STALL_RETRIES: usize = 3;

/// A generic nusb USB bulk RxWorker.
///
/// The caller is responsible for cleanup after [`run`](Self::run)
/// returns.
pub struct NusbRxWorker<N, P>
where
    N: NetStackHandle,
    P: FrameProcessor<N>,
{
    pub(crate) nsh: N,
    pub(crate) biq: Queue<RequestBuffer>,
    pub(crate) closer: Arc<WaitQueue>,
    pub(crate) processor: P,
    pub(crate) ident: <<N as NetStackHandle>::Profile as Profile>::InterfaceIdent,
    pub(crate) mtu: u16,
    pub(crate) state_notify: Option<Arc<WaitQueue>>,
}

impl<N, P> NusbRxWorker<N, P>
where
    N: NetStackHandle,
    P: FrameProcessor<N>,
{
    fn notify(&self) {
        if let Some(notify) = &self.state_notify {
            notify.wake_all();
        }
    }

    /// Run the receive loop with stall recovery.
    ///
    /// Returns `ReceiverError` when a fatal USB error occurs or
    /// the closer fires.
    pub async fn run(&mut self) -> ReceiverError {
        let mut consecutive_errs: usize = 0;

        loop {
            // Rehydrate the queue
            let pending = self.biq.pending();
            for _ in 0..(IN_FLIGHT_REQS.saturating_sub(pending)) {
                self.biq.submit(RequestBuffer::new(self.mtu as usize));
            }

            let close = self.closer.clone();

            let res = select! {
                r = self.biq.next_complete() => r,
                _clf = close.wait() => {
                    return ReceiverError::SocketClosed;
                }
            };

            if let Err(e) = res.status {
                consecutive_errs += 1;

                error!(
                    "In Worker error: {:?}, consecutive: {}",
                    e, consecutive_errs,
                );

                let recoverable = match e {
                    TransferError::Stall | TransferError::Unknown => {
                        consecutive_errs <= MAX_STALL_RETRIES
                    }
                    TransferError::Cancelled => false,
                    TransferError::Disconnected => false,
                    TransferError::Fault => false,
                };

                let fatal = if recoverable {
                    warn!("Attempting stall recovery!");

                    self.biq.cancel_all();
                    info!("Cancelled all in-flight requests");

                    for _ in 0..(IN_FLIGHT_REQS - 1) {
                        let res = self.biq.next_complete().await;
                        info!("Drain state: {:?}", res.status);
                    }

                    match self.biq.clear_halt() {
                        Ok(()) => false,
                        Err(e) => {
                            error!("Failed to clear stall: {:?}, Fatal.", e);
                            true
                        }
                    }
                } else {
                    error!(
                        "Giving up after {} errors in a row, final error: {:?}",
                        e, consecutive_errs,
                    );
                    true
                };

                if fatal {
                    error!("Fatal Error, exiting");
                    return ReceiverError::SocketClosed;
                } else {
                    info!("Potential recovery, resuming");
                    continue;
                }
            }

            if consecutive_errs != 0 {
                info!("Clearing consecutive error counter after good frame");
                consecutive_errs = 0;
            }

            trace!("Got message len {}", res.data.len());
            let changed = self
                .processor
                .process_frame(&res.data, &self.nsh, self.ident.clone());
            if changed {
                self.notify();
            }
        }
    }
}
