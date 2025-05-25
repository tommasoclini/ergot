use serde::Serialize;

use crate::Address;

pub struct InterfaceMgrHeader {}

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
