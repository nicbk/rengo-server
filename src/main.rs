use {
    rengo_server::*,
    std::{
        net::TcpListener,
        sync::Arc,
        collections::HashMap,
    },
    futures::lock::Mutex,
    log::{info, warn, debug},
    anyhow::Result,
    smol::{Async, Task},
};

fn main() -> Result<()> {
    env_logger::init();

    let num_cpus = num_cpus::get().max(1);
    let mut threadpool: Vec<Thread> = Vec::new();

    for i in 0..num_cpus {
        threadpool.push(spawn_smol_thread(&format!("Thread {}", i)[..])?);
    }

    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "0.0.0.0:8080".to_owned());

    let house = House::new(Mutex::new(HashMap::new()));

    std::thread::spawn(move || {
        handle_console_input(threadpool);
    });

    smol::block_on(async {
        let socket = match Async::<TcpListener>::bind(addr.clone()) {
            Err(e) => {
                panic!("Unable to bind to: {}: {}", addr, e);
            }

            Ok(res) => res
        };

        info!("Server started on {}", addr);

        loop {
            let connection = socket.accept().await;
            match connection {
                Err(e) => {
                    debug!("Unable to accept TCP connection: {}", e);
                }

                Ok((raw_stream, socket_addr)) => {
                    let house = Arc::clone(&house);

                    Task::spawn(async move {
                        if let Err(e) = connection_handle(raw_stream,
                                                          socket_addr,
                                                          house).await {
                            warn!("[{}]: Unable to handle connection: {}", socket_addr, e);
                        }
                    }).detach();
                }
            }
        }
    });

    Ok(())
}
