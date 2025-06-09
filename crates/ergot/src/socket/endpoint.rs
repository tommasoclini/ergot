use std::pin::{Pin, pin};

use mutex::ScopedRawMutex;
use pin_project::pin_project;
use postcard_rpc::Endpoint;
use serde::{Serialize, de::DeserializeOwned};

use crate::{FrameKind, Header, NetStack, NetStackSendError, interface_manager::InterfaceManager};

use super::{
    OwnedMessage,
    owned::{OwnedSocket, OwnedSocketHdl},
    std_bounded::{StdBoundedSocket, StdBoundedSocketHdl},
};

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
            sock: OwnedSocket::new_endpoint_req::<E>(),
        }
    }

    pub fn attach<'a, R: ScopedRawMutex + 'static, M: InterfaceManager + 'static>(
        self: Pin<&'a mut Self>,
        stack: &'static NetStack<R, M>,
    ) -> OwnedEndpointSocketHdl<'a, E, R, M> {
        let this = self.project();
        let hdl: OwnedSocketHdl<'_, E::Request, R, M> = this.sock.attach(stack);
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

pub struct OwnedEndpointSocketHdl<'a, E, R, M>
where
    E: Endpoint,
    E::Request: Serialize + DeserializeOwned + 'static,
    R: ScopedRawMutex + 'static,
    M: InterfaceManager + 'static,
{
    hdl: OwnedSocketHdl<'a, E::Request, R, M>,
}

impl<E, R, M> OwnedEndpointSocketHdl<'_, E, R, M>
where
    E: Endpoint,
    E::Request: Serialize + DeserializeOwned + 'static,
    R: ScopedRawMutex + 'static,
    M: InterfaceManager + 'static,
{
    pub async fn recv_manual(&mut self) -> OwnedMessage<E::Request> {
        self.hdl.recv().await
    }

    pub async fn serve<F: AsyncFnOnce(E::Request) -> E::Response>(
        &mut self,
        f: F,
    ) -> Result<(), NetStackSendError>
    where
        E::Response: Serialize + DeserializeOwned + 'static,
    {
        let msg = self.hdl.recv().await;
        let OwnedMessage { src, dst, t, seq } = msg;
        let resp = f(t).await;
        let hdr = Header {
            src: dst,
            dst: src,
            key: Some(E::RESP_KEY),
            seq_no: Some(seq),
            kind: FrameKind::EndpointResponse,
        };
        self.hdl.net.send_ty::<E::Response>(hdr, resp)
    }
}

// ---
// TODO: Do we need some kind of Socket trait we can use to dedupe things like this?

#[pin_project]
pub struct StdBoundedEndpointSocket<E>
where
    E: Endpoint,
    E::Request: Serialize + DeserializeOwned + 'static,
{
    #[pin]
    sock: StdBoundedSocket<E::Request>,
}

impl<E> StdBoundedEndpointSocket<E>
where
    E: Endpoint,
    E::Request: Serialize + DeserializeOwned + 'static,
{
    pub fn new(bound: usize) -> Self {
        Self {
            sock: StdBoundedSocket::new_endpoint_req::<E>(bound),
        }
    }

    pub fn attach<'a, R: ScopedRawMutex + 'static, M: InterfaceManager + 'static>(
        self: Pin<&'a mut Self>,
        stack: &'static NetStack<R, M>,
    ) -> StdBoundedEndpointSocketHdl<'a, E, R, M> {
        let this = self.project();
        let hdl: StdBoundedSocketHdl<'_, E::Request, R, M> = this.sock.attach(stack);
        StdBoundedEndpointSocketHdl { hdl }
    }
}

pub struct StdBoundedEndpointSocketHdl<'a, E, R, M>
where
    E: Endpoint,
    E::Request: Serialize + DeserializeOwned + 'static,
    R: ScopedRawMutex + 'static,
    M: InterfaceManager + 'static,
{
    hdl: StdBoundedSocketHdl<'a, E::Request, R, M>,
}

impl<E, R, M> StdBoundedEndpointSocketHdl<'_, E, R, M>
where
    E: Endpoint,
    E::Request: Serialize + DeserializeOwned + 'static,
    R: ScopedRawMutex + 'static,
    M: InterfaceManager + 'static,
{
    pub async fn recv_manual(&mut self) -> OwnedMessage<E::Request> {
        self.hdl.recv().await
    }

    pub async fn serve<F: AsyncFnOnce(E::Request) -> E::Response>(
        &mut self,
        f: F,
    ) -> Result<(), NetStackSendError>
    where
        E::Response: Serialize + DeserializeOwned + 'static,
    {
        let msg = self.hdl.recv().await;
        let OwnedMessage { src, dst, t, seq } = msg;
        let resp = f(t).await;
        let hdr = Header {
            src: dst,
            dst: src,
            key: Some(E::RESP_KEY),
            seq_no: Some(seq),
            kind: FrameKind::EndpointResponse,
        };
        self.hdl.net.send_ty::<E::Response>(hdr, resp)
    }
}
