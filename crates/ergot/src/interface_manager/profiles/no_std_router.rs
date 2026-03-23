//! No-std Direct Router profile
//!
//! A router profile for `no_std` environments that can manage up to `N`
//! directly connected downstream (edge) devices, where `N` is a const
//! generic. Uses [`heapless::Vec`] for storage and [`EdgePort`] for
//! per-interface state management.
//!
//! This is the `no_std` counterpart of [`DirectRouter`], with some
//! simplifications:
//!
//! - No seed router protocol (no `Instant`, no `rand`)
//! - No tombstoning (monotonic counters prevent stale reuse)
//! - No separate route table (linear scan over N slots, where N is small)
//!
//! Designed for embedded systems acting as a central node (apex or bridge)
//! with 2–8 downstream interfaces of potentially different types (via
//! [`multi_interface!`]).
//!
//! [`DirectRouter`]: crate::interface_manager::profiles::direct_router::DirectRouter
//! [`EdgePort`]: crate::interface_manager::edge_port::EdgePort
//! [`multi_interface!`]: crate::multi_interface

use serde::Serialize;

use crate::{
    Header, HeaderSeq, ProtocolError,
    interface_manager::{
        ConstInit, Interface, InterfaceSendError, InterfaceState, Profile, SetStateError,
        edge_port::EdgePort,
        profiles::direct_edge::{CENTRAL_NODE_ID, EDGE_NODE_ID},
    },
    logging::{debug, trace, warn},
    net_stack::NetStackHandle,
    wire_frames::de_frame,
};

/// A directly connected downstream interface slot.
struct Slot<I: Interface> {
    /// Monotonic identifier, unique for the lifetime of the router.
    ident: u8,
    /// The per-interface state machine.
    port: EdgePort<I>,
    /// The network ID assigned to this interface's link.
    net_id: u16,
}

/// A `no_std` router profile backed by a fixed-capacity [`heapless::Vec`].
///
/// `I` is the [`Interface`] type (use [`multi_interface!`] for heterogeneous
/// transports). `N` is the maximum number of simultaneously connected
/// downstream interfaces.
///
/// [`multi_interface!`]: crate::multi_interface
pub struct NoStdRouter<I: Interface, const N: usize> {
    /// Monotonic interface identifier counter. Never wraps in practice
    /// (255 registrations on an MCU is extreme).
    ident_ctr: u8,
    /// Monotonic network ID counter. Starts at 1 (0 is reserved for "local").
    net_id_ctr: u16,
    /// Active interface slots.
    slots: heapless::Vec<Slot<I>, N>,
}

/// Errors from [`NoStdRouter::register_interface`].
#[cfg_attr(feature = "defmt-v1", derive(defmt::Format))]
#[derive(Debug, PartialEq, Eq)]
pub enum RegisterError {
    /// All `N` slots are occupied.
    Full,
    /// The monotonic identifier counter has been exhausted (255 registrations).
    IdentsExhausted,
    /// The monotonic net_id counter has been exhausted.
    NetIdsExhausted,
}

/// Errors from [`NoStdRouter::deregister_interface`].
#[cfg_attr(feature = "defmt-v1", derive(defmt::Format))]
#[derive(Debug, PartialEq, Eq)]
pub enum DeregisterError {
    /// No interface with the given ident exists.
    NotFound,
}

impl<I: Interface, const N: usize> NoStdRouter<I, N> {
    /// Create a new empty router.
    pub const fn new() -> Self {
        Self {
            ident_ctr: 0,
            net_id_ctr: 1,
            slots: heapless::Vec::new(),
        }
    }

    /// Register a new downstream interface.
    ///
    /// Assigns a monotonic ident and a unique net_id. The interface starts
    /// in [`InterfaceState::Active`] with [`CENTRAL_NODE_ID`] as the local
    /// node.
    ///
    /// Returns the assigned ident on success.
    pub fn register_interface(
        &mut self,
        sink: I::Sink,
    ) -> Result<u8, RegisterError> {
        if self.slots.is_full() {
            return Err(RegisterError::Full);
        }

        let ident = self.ident_ctr;
        self.ident_ctr = self
            .ident_ctr
            .checked_add(1)
            .ok_or(RegisterError::IdentsExhausted)?;

        let net_id = self.net_id_ctr;
        // net_id 0 and 65535 are reserved
        if net_id == 0 || net_id == u16::MAX {
            return Err(RegisterError::NetIdsExhausted);
        }
        self.net_id_ctr = self
            .net_id_ctr
            .checked_add(1)
            .ok_or(RegisterError::NetIdsExhausted)?;

        let state = InterfaceState::Active {
            net_id,
            node_id: CENTRAL_NODE_ID,
        };

        // unwrap: we checked is_full above
        self.slots
            .push(Slot {
                ident,
                port: EdgePort::new_controller(sink, state),
                net_id,
            })
            .ok()
            .expect("push after is_full check");

        Ok(ident)
    }

    /// Remove a downstream interface by ident.
    pub fn deregister_interface(&mut self, ident: u8) -> Result<(), DeregisterError> {
        let pos = self
            .slots
            .iter()
            .position(|s| s.ident == ident)
            .ok_or(DeregisterError::NotFound)?;
        self.slots.swap_remove(pos);
        Ok(())
    }

    /// Get the net_id for a given ident, if it exists and is active.
    pub fn net_id_of(&self, ident: u8) -> Option<u16> {
        self.slots
            .iter()
            .find(|s| s.ident == ident)
            .map(|s| s.net_id)
    }

    /// Find an interface by destination net_id, with routing loop detection.
    fn find(
        &mut self,
        hdr: &Header,
        source: Option<u8>,
    ) -> Result<&mut EdgePort<I>, InterfaceSendError> {
        if hdr.dst.port_id == 0 && hdr.any_all.is_none() {
            return Err(InterfaceSendError::AnyPortMissingKey);
        }

        let pos = self
            .slots
            .iter()
            .position(|s| s.net_id == hdr.dst.network_id)
            .ok_or(InterfaceSendError::NoRouteToDest)?;

        let slot = &self.slots[pos];

        // Is this actually for us (central node on this link)?
        if hdr.dst.node_id == CENTRAL_NODE_ID {
            return Err(InterfaceSendError::DestinationLocal);
        }

        // Routing loop: packet came in on this same interface
        if let Some(src_ident) = source {
            if slot.ident == src_ident {
                return Err(InterfaceSendError::RoutingLoop);
            }
        }

        Ok(&mut self.slots[pos].port)
    }
}

impl<I: Interface, const N: usize> ConstInit for NoStdRouter<I, N> {
    const INIT: Self = Self::new();
}

impl<I: Interface, const N: usize> Profile for NoStdRouter<I, N> {
    type InterfaceIdent = u8;

    fn send<T: Serialize>(
        &mut self,
        hdr: &Header,
        data: &T,
    ) -> Result<(), InterfaceSendError> {
        let mut hdr = hdr.clone();
        if hdr.decrement_ttl().is_err() {
            return Err(InterfaceSendError::NoRouteToDest);
        }

        if hdr.dst.port_id == 255 {
            // Broadcast: send to all interfaces except the origin net
            if hdr.any_all.is_none() {
                return Err(InterfaceSendError::AnyPortMissingKey);
            }

            let mut any_good = false;
            for slot in self.slots.iter_mut() {
                if hdr.dst.network_id == slot.net_id {
                    continue;
                }
                let mut bhdr = hdr.clone();
                bhdr.dst.network_id = slot.net_id;
                bhdr.dst.node_id = EDGE_NODE_ID;
                any_good |= slot.port.send(&bhdr, data).is_ok();
            }
            if any_good {
                Ok(())
            } else {
                Err(InterfaceSendError::NoRouteToDest)
            }
        } else {
            let port = self.find(&hdr, None)?;
            port.send(&hdr, data)
        }
    }

    fn send_err(
        &mut self,
        hdr: &Header,
        err: ProtocolError,
        source: Option<Self::InterfaceIdent>,
    ) -> Result<(), InterfaceSendError> {
        let mut hdr = hdr.clone();
        if hdr.decrement_ttl().is_err() {
            return Err(InterfaceSendError::NoRouteToDest);
        }
        let port = self.find(&hdr, source)?;
        port.send_err(&hdr, err)
    }

    fn send_raw(
        &mut self,
        hdr: &HeaderSeq,
        data: &[u8],
        source: Self::InterfaceIdent,
    ) -> Result<(), InterfaceSendError> {
        let mut hdr = hdr.clone();
        if hdr.decrement_ttl().is_err() {
            return Err(InterfaceSendError::NoRouteToDest);
        }

        if hdr.dst.port_id == 255 {
            // Broadcast: send to all interfaces except the source
            if hdr.any_all.is_none() {
                return Err(InterfaceSendError::AnyPortMissingKey);
            }
            if self.slots.is_empty() {
                return Err(InterfaceSendError::NoRouteToDest);
            }

            let mut default_error = InterfaceSendError::RoutingLoop;
            let mut any_good = false;

            for slot in self.slots.iter_mut() {
                if source == slot.ident {
                    continue;
                }
                default_error = InterfaceSendError::NoRouteToDest;

                hdr.dst.network_id = slot.net_id;
                hdr.dst.node_id = EDGE_NODE_ID;
                any_good |= slot.port.send_raw(&hdr, data).is_ok();
            }
            if any_good {
                Ok(())
            } else {
                Err(default_error)
            }
        } else {
            let nshdr: Header = hdr.clone().into();
            let port = self.find(&nshdr, Some(source))?;
            port.send_raw(&hdr, data)
        }
    }

    fn interface_state(&mut self, ident: Self::InterfaceIdent) -> Option<InterfaceState> {
        self.slots
            .iter()
            .find(|s| s.ident == ident)
            .map(|s| s.port.state())
    }

    fn set_interface_state(
        &mut self,
        ident: Self::InterfaceIdent,
        state: InterfaceState,
    ) -> Result<(), SetStateError> {
        let slot = self
            .slots
            .iter_mut()
            .find(|s| s.ident == ident)
            .ok_or(SetStateError::InterfaceNotFound)?;
        slot.port.set_state(state)
    }
}

/// Process one received frame for a `NoStdRouter` RX worker.
///
/// This is the router-side equivalent of
/// [`direct_edge::process_frame`](crate::interface_manager::profiles::direct_edge::process_frame).
/// It rewrites the source address if needed and feeds the frame into the
/// [`NetStack`](crate::NetStack).
pub fn process_frame<N>(
    net_id: u16,
    data: &[u8],
    nsh: &N,
    ident: <<N as NetStackHandle>::Profile as Profile>::InterfaceIdent,
) where
    N: NetStackHandle,
{
    let Some(mut frame) = de_frame(data) else {
        warn!("Decode error! Ignoring frame on net_id {}", net_id);
        return;
    };

    trace!("{} got frame from {:?}", frame.hdr, ident);

    // Rewrite zero src net_id so it isn't mistaken for a local packet
    if frame.hdr.src.network_id == 0 {
        match frame.hdr.src.node_id {
            0 => {
                warn!(
                    "{}: device is sending us frames without a node id, ignoring",
                    frame.hdr
                );
                return;
            }
            CENTRAL_NODE_ID => {
                warn!(
                    "{}: device is sending us frames as us, ignoring",
                    frame.hdr
                );
                return;
            }
            EDGE_NODE_ID => {}
            _ => {
                warn!(
                    "{}: device is sending us frames with a bad node id, ignoring",
                    frame.hdr
                );
                return;
            }
        }

        frame.hdr.src.network_id = net_id;
    }

    let hdr = frame.hdr.clone();
    let nshdr: Header = hdr.clone().into();

    let res = match frame.body {
        Ok(body) => nsh.stack().send_raw(&hdr, body, ident),
        Err(e) => nsh.stack().send_err(&nshdr, e, Some(ident)),
    };

    #[allow(unused_variables)]
    match res {
        Ok(()) => {
            debug!("{}: frame delivered", hdr);
        }
        Err(e) => {
            warn!("{} recv->send error: {:?}", hdr, e);
        }
    }
}
