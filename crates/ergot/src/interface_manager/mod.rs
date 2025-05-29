use serde::Serialize;

use crate::Address;

#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InterfaceSendError {
    DestinationLocal,
    PlaceholderOhNo,
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
        data: &T,
    ) -> Result<(), InterfaceSendError>;

    fn send_raw(
        &mut self,
        src: Address,
        dst: Address,
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
        _data: &[u8],
    ) -> Result<(), InterfaceSendError> {
        if dst.net_node_any() {
            Err(InterfaceSendError::DestinationLocal)
        } else {
            Err(InterfaceSendError::PlaceholderOhNo)
        }
    }
}
