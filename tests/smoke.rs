use std::pin::pin;

use ergot::{socket::endpoint::OwnedSocket, NetStack};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use serde::{Deserialize, Serialize};

static STACK: NetStack<CriticalSectionRawMutex> = NetStack::new();

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Example {
    a: u8,
    b: u32,
}

#[test]
fn hello() {
    let socket = OwnedSocket::<Example>::new();
    let mut socket = pin!(socket);
    let hdl = socket.as_mut().attach(&STACK);
}
