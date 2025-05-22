use std::{pin::pin, time::Duration};

use ergot::{socket::endpoint::OwnedSocket, Address, NetStack};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use postcard_rpc::{endpoint, Endpoint};
use serde::{Deserialize, Serialize};
use postcard_schema::Schema;
use tokio::{spawn, time::sleep};

static STACK: NetStack<CriticalSectionRawMutex> = NetStack::new();

#[derive(Serialize, Deserialize, Debug, PartialEq, Schema)]
pub struct Example {
    a: u8,
    b: u32,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Schema)]
pub struct Other {
    a: u64,
    b: i32,
}

endpoint!(ExampleEndpoint, Example, u32, "example");
endpoint!(OtherEndpoint, Other, u32, "other");

#[tokio::test]
async fn hello() {
    let src = Address { network_id: 0, node_id: 0, port_id: 123 };
    let dst = Address { network_id: 0, node_id: 0, port_id: 0 };

    {
        let socket = OwnedSocket::<ExampleEndpoint>::new();
        let mut socket = pin!(socket);
        let mut hdl = socket.as_mut().attach(&STACK);

        let tsk = spawn(async move {
            sleep(Duration::from_millis(100)).await;

            // try sending, should fail
            STACK.send_ty::<Other>(src, dst, OtherEndpoint::REQ_KEY, Other { a: 345, b: -123 }).unwrap_err();
            // typed sending works
            STACK.send_ty::<Example>(src, dst, ExampleEndpoint::REQ_KEY, Example { a: 42, b: 789 }).unwrap();
            // raw sending works
            // (todo: wait a bit to free up space, we wont need this when we can
            // hold more than one message at a time)
            sleep(Duration::from_millis(100)).await;
            let body = postcard::to_stdvec(&Example { a: 56, b: 1234 }).unwrap();
            STACK.send_raw(src, dst, ExampleEndpoint::REQ_KEY, &body).unwrap();
        });

        let msg = hdl.recv().await;
        assert_eq!(Address { network_id: 0, node_id: 0, port_id: 123 }, msg.src);
        assert_eq!(Address { network_id: 0, node_id: 0, port_id: 0 }, msg.dst);
        assert_eq!(Example { a: 42, b: 789 }, msg.t);

        let msg = hdl.recv().await;

        assert_eq!(Address { network_id: 0, node_id: 0, port_id: 123 }, msg.src);
        assert_eq!(Address { network_id: 0, node_id: 0, port_id: 0 }, msg.dst);
        assert_eq!(Example { a: 56, b: 1234 }, msg.t);
        tsk.await.unwrap();
    }
    // The socket has now been dropped, try sending again.
    //
    // Both sends should fail.
    STACK.send_ty::<Other>(src, dst, OtherEndpoint::REQ_KEY, Other { a: 345, b: -123 }).unwrap_err();
    STACK.send_ty::<Example>(src, dst, ExampleEndpoint::REQ_KEY, Example { a: 42, b: 789 }).unwrap_err();
}
