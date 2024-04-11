use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Result};
use futures::StreamExt;
use tokio::net::TcpSocket;
use tokio::signal;
use tokio::time::{timeout, Duration};

use crate::core::common::{
    ClientResult, ClientSummary, ConnectMethod, ConnectRecord, ConnectResult, HostRecord, HostResults, IpOptions,
    IpPort, IpProtocol, LoggingOptions, PingOptions,
};
use crate::core::konst::{BIND_ADDR_IPV4, BIND_ADDR_IPV6, BIND_PORT, BUFFER_SIZE};
use crate::util::dns::resolve_host;
use crate::util::handler::{io_error_switch_handler, log_handler2, loop_handler};
use crate::util::message::{client_result_msg, client_summary_table_msg, ping_header_msg, resolved_ips_msg};
use crate::util::parser::parse_ipaddr;
use crate::util::result::{client_summary_result, get_results_map};
use crate::util::time::{calc_connect_ms, time_now_us};

#[derive(Debug)]
pub struct TcpClient {
    pub dst_ip: String,
    pub dst_port: u16,
    pub src_ipv4: Option<IpAddr>,
    pub src_ipv6: Option<IpAddr>,
    pub src_port: u16,
    pub logging_options: LoggingOptions,
    pub ping_options: PingOptions,
    pub ip_options: IpOptions,
}

impl TcpClient {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dst_ip: String,
        dst_port: u16,
        src_ipv4: Option<String>,
        src_ipv6: Option<String>,
        src_port: Option<u16>,
        logging_options: LoggingOptions,
        ping_options: PingOptions,
        ip_options: IpOptions,
    ) -> TcpClient {
        let src_ipv4 = match src_ipv4 {
            Some(x) => parse_ipaddr(&x).ok(),
            None => parse_ipaddr(BIND_ADDR_IPV4).ok(),
        };

        let src_ipv6 = match src_ipv6 {
            Some(x) => parse_ipaddr(&x).ok(),
            None => parse_ipaddr(BIND_ADDR_IPV6).ok(),
        };

        let src_port = src_port.unwrap_or(BIND_PORT);

        TcpClient {
            dst_ip,
            dst_port,
            src_ipv4,
            src_ipv6,
            src_port,
            logging_options,
            ping_options,
            ip_options,
        }
    }

    pub async fn connect(&self) -> Result<()> {
        let src_ip_port = IpPort {
            // These should never be None at this point as they are set in the TcpClient::new() constructor.
            ipv4: self.src_ipv4.unwrap(),
            ipv6: self.src_ipv6.unwrap(),
            port: self.src_port,
        };

        // Resolve the destination host to IPv4 and IPv6 addresses.
        let host_records = HostRecord::new(&self.dst_ip, self.dst_port).await;
        let hosts = vec![host_records.clone()];
        let resolved_hosts = resolve_host(hosts).await;

        // Check if the host resolved to an IPv4 or IPv6 addresses.
        // If not, return an error.
        for record in &resolved_hosts {
            match record.ipv4_sockets.is_empty() && record.ipv6_sockets.is_empty() {
                true => bail!("{} did not resolve to an IP address", record.host),
                false => {
                    let resolved_host_msg = resolved_ips_msg(record);
                    println!("{resolved_host_msg}");
                }
            }
        }

        // Filter the resolved hosts based on the IP protocol.
        let mut filtered_hosts = Vec::new();
        for record in &resolved_hosts {
            let mut record = record.clone();
            match &self.ip_options.ip_protocol {
                IpProtocol::All => {
                    filtered_hosts.push(record);
                }
                IpProtocol::V4 => {
                    record.ipv6_sockets.clear();
                    filtered_hosts.push(record);
                }
                IpProtocol::V6 => {
                    record.ipv4_sockets.clear();
                    filtered_hosts.push(record);
                }
            }
        }

        let mut results_map = get_results_map(&filtered_hosts);

        let mut count: u16 = 0;
        let mut send_count: u16 = 0;

        let ping_header = ping_header_msg(&self.dst_ip, self.dst_port, ConnectMethod::TCP);
        println!("{ping_header}");

        // This is a signal handler that listens for a Ctrl-C signal.
        // When the signal is received, it sets the cancel flag to true.
        // If the cancel flag is True we break the loop and exit the program.
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        tokio::spawn(async move {
            // TODO: this will eventually move to a channel signalling mechanism.
            signal::ctrl_c().await.unwrap();
            c.store(true, Ordering::SeqCst);
        });

        loop {
            if cancel.load(Ordering::SeqCst) {
                break;
            }
            match loop_handler(count, self.ping_options.repeat, self.ping_options.interval).await {
                true => break,
                false => count += 1,
            }

            let host_results: Vec<HostResults> = futures::stream::iter(resolved_hosts.clone())
                .map(|host_record| {
                    let src_ip_port = src_ip_port.clone();
                    async move {
                        //
                        process_host(src_ip_port, host_record, self.ping_options, self.ip_options).await
                    }
                })
                .buffer_unordered(BUFFER_SIZE)
                .collect()
                .await;

            for host in host_results {
                for result in host.results {
                    results_map
                        // This should never fail
                        .get_mut(&host.host)
                        .unwrap()
                        // This should never fail
                        .get_mut(&result.destination)
                        .unwrap()
                        .push(result.time);

                    let success_msg = client_result_msg(&result);
                    log_handler2(&result, &success_msg, &self.logging_options).await;
                }
            }

            send_count += 1;
        }

        let mut client_results: Vec<ClientResult> = Vec::new();
        for (_, addrs) in results_map {
            for (addr, latencies) in addrs {
                let client_summary = ClientSummary { send_count, latencies };
                let summary_msg = client_summary_result(&addr, ConnectMethod::TCP, client_summary);
                client_results.push(summary_msg)
            }
        }
        client_results.sort_by_key(|x| x.destination.to_owned());

        let summary_table = client_summary_table_msg(&self.dst_ip, self.dst_port, ConnectMethod::TCP, &client_results);
        println!("{}", summary_table);

        Ok(())
    }
}

async fn process_host(
    src_ip_port: IpPort,
    host_record: HostRecord,
    ping_options: PingOptions,
    ip_options: IpOptions,
) -> HostResults {
    // Create a vector of sockets based on the IP protocol.
    let sockets = match ip_options.ip_protocol {
        IpProtocol::All => [host_record.ipv4_sockets, host_record.ipv6_sockets].concat(),
        IpProtocol::V4 => host_record.ipv4_sockets,
        IpProtocol::V6 => host_record.ipv6_sockets,
    };

    let results: Vec<ConnectRecord> = futures::stream::iter(sockets)
        .map(|dst_socket| {
            let src_ip_port = src_ip_port.clone();
            async move {
                //
                connect_host(src_ip_port, dst_socket, ping_options).await
            }
        })
        .buffer_unordered(BUFFER_SIZE)
        .collect()
        .await;

    HostResults {
        host: host_record.host,
        results,
    }
}

async fn connect_host(src: IpPort, dst_socket: SocketAddr, ping_options: PingOptions) -> ConnectRecord {
    let (bind_addr, src_socket) = match &dst_socket.is_ipv4() {
        // Bind the source socket to the same IP Version as the destination socket.
        true => {
            let bind_ipv4_addr = SocketAddr::new(src.ipv4, src.port);
            let socket = get_tcp_socket(bind_ipv4_addr).ok();
            (bind_ipv4_addr, socket)
        }
        false => {
            let bind_ipv6_addr = SocketAddr::new(src.ipv6, src.port);
            let socket = get_tcp_socket(bind_ipv6_addr).ok();
            (bind_ipv6_addr, socket)
        }
    };

    // If the source socket is None, we could not bind to the socket.
    if src_socket.is_none() {
        return ConnectRecord {
            result: ConnectResult::BindError,
            protocol: ConnectMethod::TCP,
            source: bind_addr.to_string(),
            destination: dst_socket.to_string(),
            time: -1.0,
            success: false,
            error_msg: Some("Error binding to socket".to_owned()),
        };
    }
    // Unwrap the socket because we have already checked that it is not None.
    let src_socket = src_socket.unwrap();

    let local_addr = src_socket
        .local_addr()
        // This should never fail because we always
        // pass a bound socket.
        .unwrap_or_else(|_| panic!("ERROR GETTING TCP SOCKET LOCAL ADDRESS"))
        .to_string();

    let mut conn_record = ConnectRecord {
        result: ConnectResult::Unknown,
        protocol: ConnectMethod::TCP,
        source: local_addr,
        destination: dst_socket.to_string(),
        time: -1.0,
        success: false,
        error_msg: None,
    };

    // record timestamp before connection
    let pre_conn_timestamp = time_now_us();

    let tick = Duration::from_millis(ping_options.timeout.into());
    match timeout(tick, src_socket.connect(dst_socket)).await {
        Ok(s) => match s {
            Ok(stream) => {
                // Update conn record
                // Calculate the round trip time
                let post_conn_timestamp = time_now_us();
                let connection_time = calc_connect_ms(pre_conn_timestamp, post_conn_timestamp);

                conn_record.source = stream
                    .local_addr()
                    // This should never fail. If we have a TCP stream,
                    // we should have always have a local address.
                    .unwrap_or_else(|_| panic!("ERROR GETTING TCP STREAM LOCAL ADDRESS"))
                    .to_string();
                conn_record.success = true;
                conn_record.result = ConnectResult::Pong;
                conn_record.time = connection_time;

                // TODO:
                // send/receive nk message
            }
            // Connection timeout
            Err(e) => {
                let error_msg = e.to_string();
                conn_record.result = io_error_switch_handler(e);
                conn_record.error_msg = Some(error_msg);
            }
        },
        // Timeout error
        Err(e) => {
            let error_msg = e.to_string();
            conn_record.result = io_error_switch_handler(e.into());
            conn_record.error_msg = Some(error_msg);
        }
    };
    conn_record
}

fn get_tcp_socket(bind_addr: SocketAddr) -> Result<TcpSocket> {
    let socket = match bind_addr.is_ipv4() {
        true => TcpSocket::new_v4()?,
        false => TcpSocket::new_v6()?,
    };
    socket.bind(bind_addr)?;
    Ok(socket)
}
