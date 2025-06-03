use postcard_rpc::Key;
use serde::Serialize;
use crate::Address;

pub mod std_tcp_router;

#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InterfaceSendError {
    /// Refusing to send local destination remotely
    DestinationLocal,
    /// Interface Manager does not know how to route to requested destination
    NoRouteToDest,
    /// Interface Manager found a destination interface, but that interface
    /// was full in space/slots
    InterfaceFull,
    /// TODO: Remove
    PlaceholderOhNo,
    /// Destination was an "any" port, but a key was not provided
    AnyPortMissingKey,
}

pub trait ConstInit {
    const INIT: Self;
}

// An interface send is very similar to a socket send, with the exception
// that interface sends are ALWAYS a serializing operation (or requires
// serialization has already been done), which means we don't need to
// differentiate between "send owned" and "send borrowed". The exception
// to this is "send raw", where serialization has already been done, e.g.
// if we are routing a packet.
pub trait InterfaceManager {
    fn send<T: Serialize>(
        &mut self,
        src: Address,
        dst: Address,
        key: Option<Key>,
        data: &T,
    ) -> Result<(), InterfaceSendError>;

    fn send_raw(
        &mut self,
        src: Address,
        dst: Address,
        key: Option<Key>,
        data: &[u8],
    ) -> Result<(), InterfaceSendError>;
}

pub struct NullInterfaceManager {
    _priv: (),
}

impl ConstInit for NullInterfaceManager {
    const INIT: Self = Self { _priv: () };
}

impl InterfaceManager for NullInterfaceManager {
    fn send<T: Serialize>(
        &mut self,
        _src: Address,
        dst: Address,
        _key: Option<Key>,
        _data: &T,
    ) -> Result<(), InterfaceSendError> {
        if dst.net_node_any() {
            Err(InterfaceSendError::DestinationLocal)
        } else {
            Err(InterfaceSendError::PlaceholderOhNo)
        }
    }

    fn send_raw(
        &mut self,
        _src: Address,
        dst: Address,
        _key: Option<Key>,
        _data: &[u8],
    ) -> Result<(), InterfaceSendError> {
        if dst.net_node_any() {
            Err(InterfaceSendError::DestinationLocal)
        } else {
            Err(InterfaceSendError::PlaceholderOhNo)
        }
    }
}
