use ergot::{
    FrameKind,
    logging::log_v0_4::LogSink,
    toolkits::tokio_tcp::{EdgeStack, new_std_queue, new_target_stack, register_edge_interface},
    topic,
    traits::Endpoint,
    well_known::{DeviceInfo, ErgotSeedRouterAssignmentEndpoint, NameRequirement, SocketQuery},
};
use log::{info, warn};
use tokio::{net::TcpStream, select, time::sleep};

use std::{io, pin::pin, time::Duration};

topic!(YeetTopic, u64, "topic/yeet");

#[tokio::main]
async fn main() -> io::Result<()> {
    let queue = new_std_queue(4096);
    let stack: EdgeStack = new_target_stack(&queue, 1024);
    let socket = TcpStream::connect("127.0.0.1:2025").await.unwrap();
    let port = socket.local_addr().unwrap().port();
    // let logger = Box::new(LogSink::new(stack.clone()));
    // let logger = Box::leak(logger);
    // logger.register_static(log::LevelFilter::Info);
    env_logger::init();

    tokio::task::spawn(basic_services(stack.clone(), port));
    // tokio::task::spawn(yeeter(stack.clone()));
    // for i in 1..4 {
    //     tokio::task::spawn(yeet_listener(stack.clone(), i));
    // }

    register_edge_interface(&stack, socket, &queue)
        .await
        .unwrap();
    loop {
        info!("Hello :)");
        // tokio::time::sleep(Duration::from_secs(1)).await;
        let query = SocketQuery {
            key: ErgotSeedRouterAssignmentEndpoint::REQ_KEY.to_bytes(),
            nash_req: NameRequirement::Any,
            frame_kind: FrameKind::ENDPOINT_REQ,
            broadcast: false,
        };
        log::error!("Do Disco!");
        let res = stack
            .discovery()
            .discover_sockets(4, Duration::from_secs(1), &query)
            .await;
        if res.is_empty() {
            log::warn!("No Disco");
            sleep(Duration::from_secs(1)).await;
            continue;
        }

        log::warn!("DISCO SAID: {:?}", res);
        let resp = stack
            .endpoints()
            .request::<ErgotSeedRouterAssignmentEndpoint>(res[0].address, &(), None)
            .await;
        log::warn!("GOT: {:?}", resp);
        break;
    }

    loop {
        info!("Done :)");
        sleep(Duration::from_secs(1)).await;
    }
}

async fn basic_services(stack: EdgeStack, port: u16) {
    let info = DeviceInfo {
        name: Some("Ergot client".try_into().unwrap()),
        description: Some("An Ergot Client Device".try_into().unwrap()),
        unique_id: port.into(),
    };
    let do_pings = stack.services().ping_handler::<4>();
    let do_info = stack.services().device_info_handler::<4>(&info);

    select! {
        _ = do_pings => {}
        _ = do_info => {}
    }
}

async fn yeeter(stack: EdgeStack) {
    let mut ctr = 0;
    tokio::time::sleep(Duration::from_secs(3)).await;
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        warn!("Sending broadcast message");
        stack.topics().broadcast::<YeetTopic>(&ctr, None).unwrap();
        ctr += 1;
    }
}

async fn yeet_listener(stack: EdgeStack, id: u8) {
    let subber = stack.topics().heap_bounded_receiver::<YeetTopic>(64, None);
    let subber = pin!(subber);
    let mut hdl = subber.subscribe();

    loop {
        let msg = hdl.recv().await;
        info!("{}: Listener id:{} got {}", msg.hdr, id, msg.t);
    }
}
