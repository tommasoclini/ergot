use postcard_rpc::Key;
use serde::Serialize;

use crate::Address;

use super::{ConstInit, InterfaceManager, InterfaceSendError};


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
