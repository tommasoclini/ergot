//! "Edge" device profile
//!
//! Edge devices are the second simplest device profile, and are intended for devices
//! that are on the "edge" of a network, e.g. they have a single upstream connection
//! to a bridge or seed router.
//!
//! These devices use as many tricks as possible to be as simple as possible. They
//! initially start not knowing their network ID, and if a packet is sent to them,
//! they assume the destination net ID is their net ID. They will also blindly send
//! any outgoing packets, rather than trying to determine whether that packet is
//! actually routable to a node on the network.

use crate::logging::{debug, warn};

use serde::Serialize;

#[cfg(any(feature = "embedded-io-async-v0_6", feature = "embedded-io-async-v0_7"))]
pub mod eio;

#[cfg(feature = "embassy-usb-v0_5")]
pub mod eusb_0_5;

#[cfg(feature = "embassy-usb-v0_6")]
pub mod eusb_0_6;

#[cfg(feature = "tokio-std")]
pub mod tokio_tcp;

#[cfg(feature = "tokio-std")]
pub mod tokio_udp;

#[cfg(feature = "tokio-std")]
pub mod tokio_stream;

#[cfg(feature = "embassy-net-v0_7")]
pub mod embassy_net_udp_0_7;

use crate::{
    Header, HeaderSeq, ProtocolError,
    interface_manager::{
        Interface, InterfaceSendError, InterfaceState, Profile, SetStateError, edge_port::EdgePort,
    },
    net_stack::NetStackHandle,
    wire_frames::de_frame,
};

pub use crate::interface_manager::edge_port::{CENTRAL_NODE_ID, EDGE_NODE_ID};

pub enum SetNetIdError {
    CantSetZero,
    NoActiveSink,
}

/// Edge device profile backed by a single [`EdgePort`].
pub struct DirectEdge<I: Interface> {
    port: EdgePort<I>,
    /// Closer for signaling workers to stop. Set by `register_*_stream`,
    /// closed when the interface transitions to `Down`.
    #[cfg(feature = "std")]
    closer: Option<std::sync::Arc<maitake_sync::WaitQueue>>,
}

impl<I: Interface> DirectEdge<I> {
    pub const fn new_target(sink: I::Sink) -> Self {
        Self {
            port: EdgePort::new_target(sink),
            #[cfg(feature = "std")]
            closer: None,
        }
    }

    pub const fn new_controller(sink: I::Sink, state: InterfaceState) -> Self {
        Self {
            port: EdgePort::new_controller(sink, state),
            #[cfg(feature = "std")]
            closer: None,
        }
    }

    /// Tear down the interface: stop any running workers and transition to `Down`.
    ///
    /// Call this before re-opening a transport to ensure the old workers
    /// release the transport resource (e.g., serial port).
    #[cfg(feature = "std")]
    pub fn teardown(&mut self) {
        if let Some(closer) = self.closer.take() {
            closer.close();
        }
        let _ = self.port.set_state(InterfaceState::Down);
    }

    /// Store a closer WaitQueue so that workers are signaled when the
    /// interface transitions to `Down` or when a new stream is registered.
    #[cfg(feature = "std")]
    pub fn set_closer(&mut self, closer: std::sync::Arc<maitake_sync::WaitQueue>) {
        // Close any existing workers before replacing
        if let Some(old) = self.closer.take() {
            old.close();
        }
        self.closer = Some(closer);
    }
}

impl<I: Interface> Profile for DirectEdge<I> {
    type InterfaceIdent = ();

    fn send<T: Serialize>(&mut self, hdr: &Header, data: &T) -> Result<(), InterfaceSendError> {
        let mut hdr = hdr.clone();
        hdr.decrement_ttl()?;
        self.port.send(&hdr, data)
    }

    fn send_err(
        &mut self,
        hdr: &Header,
        err: ProtocolError,
        source: Option<Self::InterfaceIdent>,
    ) -> Result<(), InterfaceSendError> {
        if source.is_some() {
            return Err(InterfaceSendError::RoutingLoop);
        }
        let mut hdr = hdr.clone();
        hdr.decrement_ttl()?;
        self.port.send_err(&hdr, err)
    }

    fn send_raw(
        &mut self,
        _hdr: &HeaderSeq,
        _data: &[u8],
        _source: Self::InterfaceIdent,
    ) -> Result<(), InterfaceSendError> {
        // As a DirectEdge, we should never accept a raw message, as that must have
        // come from us.
        Err(InterfaceSendError::RoutingLoop)
    }

    fn interface_state(&mut self, _ident: ()) -> Option<InterfaceState> {
        Some(self.port.state())
    }

    fn set_interface_state(
        &mut self,
        _ident: (),
        state: InterfaceState,
    ) -> Result<(), SetStateError> {
        self.port.set_state(state)
    }
}

/// Process one rx worker frame for direct edge workers
pub fn process_frame<N>(
    net_id: &mut Option<u16>,
    data: &[u8],
    nsh: &N,
    ident: <<N as NetStackHandle>::Profile as Profile>::InterfaceIdent,
) where
    N: NetStackHandle,
{
    let Some(mut frame) = de_frame(data) else {
        warn!(
            "Decode error! Ignoring frame on net_id {}",
            net_id.unwrap_or(0)
        );
        return;
    };

    debug!("{}: Got Frame!", frame.hdr);

    let take_net = net_id.is_none()
        || net_id.is_some_and(|n| frame.hdr.dst.network_id != 0 && n != frame.hdr.dst.network_id);

    if take_net {
        let ok = nsh.stack().manage_profile(|im| {
            im.set_interface_state(
                ident.clone(),
                InterfaceState::Active {
                    net_id: frame.hdr.dst.network_id,
                    node_id: EDGE_NODE_ID,
                },
            )
            .is_ok()
        });
        if ok {
            *net_id = Some(frame.hdr.dst.network_id);
        } else {
            warn!("Failed to set interface state from frame (wrong node_id?), dropping");
            return;
        }
    }

    // If the message comes in and has a src net_id of zero,
    // we should rewrite it so it isn't later understood as a
    // local packet.
    //
    // TODO: accept any packet if we don't have a net_id yet?
    if let Some(net) = net_id.as_ref()
        && frame.hdr.src.network_id == 0
    {
        if frame.hdr.src.node_id == 0 {
            warn!("Dropping frame with src node_id 0 (stale or local packet received remotely)");
            return;
        }
        if frame.hdr.src.node_id == EDGE_NODE_ID {
            warn!(
                "Dropping frame with src node_id {} (spoofed as us)",
                EDGE_NODE_ID
            );
            return;
        }

        frame.hdr.src.network_id = *net;
    }

    // TODO: if the destination IS self.net_id, we could rewrite the
    // dest net_id as zero to avoid a pass through the interface manager.
    //
    // If the dest is 0, should we rewrite the dest as self.net_id? This
    // is the opposite as above, but I dunno how that will work with responses
    let res = match frame.body {
        Ok(body) => nsh.stack().send_raw(&frame.hdr, body, ident),
        Err(e) => {
            // send_err requires a Header instead of a HeaderSeq, so we convert it
            let nshdr: Header = frame.hdr.clone().into();
            nsh.stack().send_err(&nshdr, e, Some(ident))
        }
    };

    #[allow(unused_variables)]
    match res {
        Ok(()) => {}
        Err(e) => {
            // TODO: match on error, potentially try to send NAK?
            warn!("send error: {:?}", e);
        }
    }
}
