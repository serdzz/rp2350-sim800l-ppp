//! Демонстрационная нагрузка поверх поднятого PPP-канала: DNS + HTTP GET.
//!
//! Здесь же удобно писать свою логику — `Stack<'static>` даёт полноценные
//! TCP/UDP/DNS сокеты `embassy-net`.

use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpEndpoint, Stack};
use embassy_time::{Duration, Timer};
use embedded_io_async::Write;

use crate::config;

#[embassy_executor::task]
pub async fn demo_task(stack: Stack<'static>) -> ! {
    let mut rx_buffer = [0u8; 2048];
    let mut tx_buffer = [0u8; 1024];
    let mut buf = [0u8; 1024];

    loop {
        // Ждём, пока PPP отдаст IPCP-конфигурацию и стек станет рабочим.
        stack.wait_config_up().await;

        if let Some(cfg) = stack.config_v4() {
            info!("NET: адрес {}", cfg.address);
            for dns in cfg.dns_servers.iter() {
                info!("NET: DNS {}", dns);
            }
        }

        match run_once(stack, &mut rx_buffer, &mut tx_buffer, &mut buf).await {
            Ok(()) => info!("NET: демо-запрос выполнен"),
            Err(()) => warn!("NET: демо-запрос не удался"),
        }

        Timer::after(Duration::from_secs(30)).await;
    }
}

async fn run_once(
    stack: Stack<'static>,
    rx_buffer: &mut [u8],
    tx_buffer: &mut [u8],
    buf: &mut [u8],
) -> Result<(), ()> {
    let addrs = stack
        .dns_query(config::DEMO_HOST, DnsQueryType::A)
        .await
        .map_err(|e| warn!("NET: DNS не ответил: {:?}", e))?;
    let addr = *addrs.first().ok_or_else(|| warn!("NET: пустой DNS-ответ"))?;
    info!("NET: {} -> {}", config::DEMO_HOST, addr);

    let mut socket = TcpSocket::new(stack, rx_buffer, tx_buffer);
    // На GPRS ходят задержки в сотни миллисекунд — таймаут должен быть щедрым.
    socket.set_timeout(Some(Duration::from_secs(20)));

    socket
        .connect(IpEndpoint::new(addr, config::DEMO_PORT))
        .await
        .map_err(|e| warn!("NET: connect: {:?}", e))?;
    info!("NET: соединение установлено");

    let mut request = heapless::String::<128>::new();
    let _ = core::fmt::Write::write_fmt(
        &mut request,
        format_args!(
            "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            config::DEMO_HOST
        ),
    );
    socket
        .write_all(request.as_bytes())
        .await
        .map_err(|e| warn!("NET: write: {:?}", e))?;

    let n = socket
        .read(buf)
        .await
        .map_err(|e| warn!("NET: read: {:?}", e))?;
    info!(
        "NET: ответ ({} байт): {}",
        n,
        core::str::from_utf8(&buf[..n.min(96)]).unwrap_or("<не utf8>")
    );

    socket.close();
    Ok(())
}
