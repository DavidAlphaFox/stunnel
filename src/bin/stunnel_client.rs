#[macro_use]
extern crate log;
extern crate async_std;
extern crate getopts;
extern crate stunnel;

use std::env;
use std::net::Shutdown;
use std::net::ToSocketAddrs;
use std::str::from_utf8;
use std::vec::Vec;

use async_std::net::TcpListener;
use async_std::net::TcpStream;
use async_std::prelude::*;
use async_std::task;

use stunnel::client::*;
use stunnel::cryptor::Cryptor;
use stunnel::logger;
use stunnel::socks5;

async fn process_read(stream: &mut &TcpStream, mut write_port: TunnelWritePort) {
    loop {
        let mut buf = vec![0; 1024];
        match stream.read(&mut buf).await {
            Ok(0) => {
                let _ = stream.shutdown(Shutdown::Read);
                write_port.shutdown_write().await;
                write_port.drop().await;
                break;
            }

            Ok(n) => {
                buf.truncate(n);
                write_port.write(buf).await;
            }

            Err(_) => {
                let _ = stream.shutdown(Shutdown::Both);
                write_port.close().await;
                break;
            }
        }
    }
}

async fn process_write(stream: &mut &TcpStream, mut read_port: TunnelReadPort) {
    loop {
        let buf = match read_port.read().await {
            TunnelPortMsg::Data(buf) => buf,

            TunnelPortMsg::ShutdownWrite => {
                let _ = stream.shutdown(Shutdown::Write);
                read_port.drain();
                read_port.drop().await;
                break;
            }

            _ => {
                let _ = stream.shutdown(Shutdown::Both);
                read_port.drain();
                read_port.close().await;
                break;
            }
        };

        if stream.write_all(&buf).await.is_err() {
            let _ = stream.shutdown(Shutdown::Both);
            read_port.drain();
            read_port.close().await;
            break;
        }
    }
}

async fn run_tunnel_port(
    mut stream: TcpStream,
    mut read_port: TunnelReadPort,
    mut write_port: TunnelWritePort,
) {
    match socks5::handshake(&mut stream).await {
        Ok(socks5::Destination::Address(addr)) => {
            let mut buf = Vec::new();
            let _ = std::io::Write::write_fmt(&mut buf, format_args!("{}", addr));
            write_port.connect(buf).await;
        }

        Ok(socks5::Destination::DomainName(domain_name, port)) => {
            write_port.connect_domain_name(domain_name, port).await;
        }

        _ => {
            return write_port.close().await;
        }
    }

    let addr = match read_port.read().await {
        TunnelPortMsg::ConnectOk(buf) => from_utf8(&buf).unwrap().to_socket_addrs().unwrap().nth(0),

        _ => None,
    };

    let success = match addr {
        Some(addr) => socks5::destination_connected(&mut stream, addr)
            .await
            .is_ok(),
        None => socks5::destination_unreached(&mut stream).await.is_ok() && false,
    };

    if success {
        let (reader, writer) = &mut (&stream, &stream);
        let r = process_read(reader, write_port);
        let w = process_write(writer, read_port);
        let _ = r.join(w).await;
    } else {
        let _ = stream.shutdown(Shutdown::Both);
        read_port.drain();
        write_port.close().await;
    }
}

fn run_tunnels(
    listen_addr: String,
    server_addr: String,
    count: u32,
    key: Vec<u8>,
    enable_ucp: bool,
) {
    task::block_on(async move {
        let mut tunnels = Vec::new();
        if enable_ucp {
            let tunnel = UcpTunnel::new(0, server_addr.clone(), key.clone());
            tunnels.push(tunnel);
        } else {
            for i in 0..count {
                let tunnel = TcpTunnel::new(i, server_addr.clone(), key.clone());
                tunnels.push(tunnel);
            }
        }

        let mut index = 0;
        let listener = TcpListener::bind(listen_addr.as_str()).await.unwrap();
        let mut incoming = listener.incoming();

        while let Some(stream) = incoming.next().await {
            match stream {
                Ok(stream) => {
                    {
                        let tunnel: &mut Tunnel = tunnels.get_mut(index).unwrap();
                        let (write_port, read_port) = tunnel.open_port().await;
                        task::spawn(async move {
                            run_tunnel_port(stream, read_port, write_port).await;
                        });
                    }

                    index = (index + 1) % tunnels.len();
                }

                Err(_) => {}
            }
        }
    });
}

fn main() {
    let args: Vec<_> = env::args().collect();
    let program = args[0].clone();

    let mut opts = getopts::Options::new();
    opts.reqopt("s", "server", "server address", "server-address");
    opts.reqopt("k", "key", "secret key", "key");
    opts.optopt("c", "tunnel-count", "tunnel count", "tunnel-count");
    opts.optopt("l", "listen", "listen address", "listen-address");
    opts.optopt("", "log", "log path", "log-path");
    opts.optflag("", "enable-ucp", "enable ucp");

    let matches = match opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(_) => {
            println!("{}", opts.short_usage(&program));
            return;
        }
    };

    let server_addr = matches.opt_str("s").unwrap();
    let tunnel_count = matches.opt_str("c").unwrap_or(String::new());
    let key = matches.opt_str("k").unwrap().into_bytes();
    let log_path = matches.opt_str("log").unwrap_or(String::new());
    let enable_ucp = matches.opt_present("enable-ucp");
    let listen_addr = matches.opt_str("l").unwrap_or("127.0.0.1:1080".to_string());
    let (min, max) = Cryptor::key_size_range();

    if key.len() < min || key.len() > max {
        println!("key length must in range [{}, {}]", min, max);
        return;
    }

    let count: u32 = match tunnel_count.parse() {
        Err(_) | Ok(0) => 1,
        Ok(count) => count,
    };

    logger::init(log::Level::Info, log_path, 1, 2000000).unwrap();
    info!("starting up");

    run_tunnels(listen_addr, server_addr, count, key, enable_ucp);
}
