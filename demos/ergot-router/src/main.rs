use ergot::{interface_manager::std_tcp::{register_interface, StdTcpIm}, NetStack};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use tokio::net::TcpListener;

use std::io;

static STACK: NetStack<CriticalSectionRawMutex, StdTcpIm> = NetStack::new();

#[tokio::main]
async fn main() -> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:2025").await?;

    loop {
        let (socket, addr) = listener.accept().await?;
        println!("Connect {addr:?}");
        let hdl = register_interface(&STACK, socket).unwrap();
        tokio::task::spawn(async move {
            let res = hdl.run().await;
            println!("END: {res:?}");
        });
    }
}
