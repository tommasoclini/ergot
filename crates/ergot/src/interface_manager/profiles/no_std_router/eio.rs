//! Embedded-IO RX worker for [`NoStdRouter`]
//!
//! Reads bytes from an `embedded_io_async::Read` source, decodes
//! COBS frames, and feeds them into the [`NetStack`] via
//! [`process_frame`].
//!
//! Unlike the [`DirectEdge`] RX worker, this worker does not need to
//! discover its net_id — the router has already assigned one at
//! registration time.
//!
//! [`NoStdRouter`]: super::NoStdRouter
//! [`NetStack`]: crate::NetStack
//! [`DirectEdge`]: crate::interface_manager::profiles::direct_edge

use cobs_acc::{CobsAccumulator, FeedResult};

use crate::{
    eio::Read,
    interface_manager::{InterfaceState, Profile},
    net_stack::NetStackHandle,
};

use super::process_frame;

/// RX worker for a single downstream interface on a [`NoStdRouter`].
///
/// Each downstream interface spawns one `RxWorker` as an async task.
/// The worker reads from the physical transport, decodes COBS frames,
/// and delivers them to the netstack.
///
/// On drop, the interface state is set to [`InterfaceState::Down`].
///
/// [`NoStdRouter`]: super::NoStdRouter
pub struct RxWorker<N, R>
where
    N: NetStackHandle,
    R: Read,
    <<N as NetStackHandle>::Profile as Profile>::InterfaceIdent: From<u8>,
{
    nsh: N,
    rx: R,
    net_id: u16,
    ident: u8,
}

impl<N, R> RxWorker<N, R>
where
    N: NetStackHandle,
    R: Read,
    <<N as NetStackHandle>::Profile as Profile>::InterfaceIdent: From<u8>,
{
    /// Create a new RX worker.
    ///
    /// `net_id` and `ident` come from [`NoStdRouter::register_interface`]
    /// and [`NoStdRouter::net_id_of`].
    ///
    /// [`NoStdRouter::register_interface`]: super::NoStdRouter::register_interface
    /// [`NoStdRouter::net_id_of`]: super::NoStdRouter::net_id_of
    pub fn new(nsh: N, rx: R, net_id: u16, ident: u8) -> Self {
        Self {
            nsh,
            rx,
            net_id,
            ident,
        }
    }

    /// Run the receive loop.
    ///
    /// `frame_buf` should be at least MTU + overhead bytes.
    /// `scratch_buf` is used for individual read chunks (e.g. 64–256 bytes).
    ///
    /// Returns when the underlying transport returns an error.
    /// On return (or drop), the interface is set to Down.
    pub async fn run(
        &mut self,
        frame_buf: &mut [u8],
        scratch_buf: &mut [u8],
    ) -> Result<(), R::Error> {
        let res = self.run_inner(frame_buf, scratch_buf).await;
        self.nsh.stack().manage_profile(|im| {
            _ = im.set_interface_state(self.ident.into(), InterfaceState::Down);
        });
        res
    }

    async fn run_inner(
        &mut self,
        frame_buf: &mut [u8],
        scratch_buf: &mut [u8],
    ) -> Result<(), R::Error> {
        let mut acc = CobsAccumulator::new(frame_buf);
        let Self {
            nsh,
            rx,
            net_id,
            ident,
        } = self;
        'outer: loop {
            let used = rx.read(scratch_buf).await?;
            let mut window = &mut scratch_buf[..used];

            loop {
                match acc.feed_raw(window) {
                    FeedResult::Consumed => continue 'outer,
                    FeedResult::OverFull(remaining) => {
                        window = remaining;
                    }
                    FeedResult::DecodeError(remaining) => {
                        window = remaining;
                    }
                    FeedResult::Success { data, remaining }
                    | FeedResult::SuccessInput { data, remaining } => {
                        process_frame(*net_id, data, nsh, (*ident).into());
                        window = remaining;
                    }
                }
            }
        }
    }
}

impl<N, R> Drop for RxWorker<N, R>
where
    N: NetStackHandle,
    R: Read,
    <<N as NetStackHandle>::Profile as Profile>::InterfaceIdent: From<u8>,
{
    fn drop(&mut self) {
        self.nsh.stack().manage_profile(|im| {
            _ = im.set_interface_state(self.ident.into(), InterfaceState::Down);
        });
    }
}
