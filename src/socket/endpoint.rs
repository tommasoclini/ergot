use std::pin::{Pin, pin};

use mutex::ScopedRawMutex;
use pin_project::pin_project;
use postcard_rpc::Endpoint;
use serde::{Serialize, de::DeserializeOwned};

use crate::NetStack;

use super::owned::{OwnedMessage, OwnedSocket, OwnedSocketHdl};

#[pin_project]
pub struct OwnedEndpointSocket<E>
where
    E: Endpoint,
    E::Request: Serialize + DeserializeOwned + 'static,
{
    #[pin]
    sock: OwnedSocket<E::Request>,
}

impl<E> OwnedEndpointSocket<E>
where
    E: Endpoint,
    E::Request: Serialize + DeserializeOwned + 'static,
{
    pub const fn new() -> Self {
        Self {
            sock: OwnedSocket::new(E::REQ_KEY),
        }
    }

    pub fn attach<'a, R: ScopedRawMutex + 'static>(
        self: Pin<&'a mut Self>,
        stack: &'static NetStack<R>,
    ) -> OwnedEndpointSocketHdl<'a, E, R> {
        let this = self.project();
        let hdl: OwnedSocketHdl<'_, E::Request, R> = this.sock.attach(stack);
        OwnedEndpointSocketHdl { hdl }
    }
}

impl<E> Default for OwnedEndpointSocket<E>
where
    E: Endpoint,
    E::Request: Serialize + DeserializeOwned + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

pub struct OwnedEndpointSocketHdl<'a, E, R>
where
    E: Endpoint,
    E::Request: Serialize + DeserializeOwned + 'static,
    R: ScopedRawMutex + 'static,
{
    hdl: OwnedSocketHdl<'a, E::Request, R>,
}

impl<E, R> OwnedEndpointSocketHdl<'_, E, R>
where
    E: Endpoint,
    E::Request: Serialize + DeserializeOwned + 'static,
    R: ScopedRawMutex + 'static,
{
    pub async fn recv_manual(&mut self) -> OwnedMessage<E::Request> {
        self.hdl.recv().await
    }

    pub async fn serve<F: AsyncFnOnce(E::Request) -> E::Response>(&mut self, f: F) -> Result<(), ()>
    where
        E::Response: Serialize + DeserializeOwned + 'static,
    {
        let msg = self.hdl.recv().await;
        let OwnedMessage { src, dst, t } = msg;
        let resp = f(t).await;
        self.hdl
            .net
            .send_ty::<E::Response>(dst, src, E::RESP_KEY, resp)
    }
}
