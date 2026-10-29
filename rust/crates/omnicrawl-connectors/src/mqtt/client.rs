//! 阻塞式 MQTT 客户端：TCP / TLS 建连、订阅、发布与心跳。
//!
//! 传输层写法与飞书长连接同源（手写帧 + rustls + webpki 根证书），不引入 MQTT 生态依赖。
//! 只有一条连接、一个读线程：问答式（订阅 → 读 → 决策 → 发布）不需要并发读写，
//! 单一所有者反而省掉了跨线程同步与半包交错的麻烦。

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use super::codec::{self, Packet};
use super::config::MqttConfig;

/// 单次读的等待粒度：主循环据此回到心跳与窗口检查。
pub const READ_SLICE: Duration = Duration::from_millis(300);

/// 解析后的 broker 地址。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerAddress {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

/// 拆 mqtt://host:port、mqtts://host:8883、tcp://、tls:// 四种写法。
pub fn parse_broker_url(url: &str) -> Result<BrokerAddress, String> {
    let trimmed = url.trim();
    let (scheme, rest) = trimmed
        .split_once("://")
        .ok_or_else(|| format!("broker 地址缺少协议前缀（mqtt:// 或 mqtts://）：{trimmed}"))?;
    let tls = match scheme.to_ascii_lowercase().as_str() {
        "mqtt" | "tcp" => false,
        "mqtts" | "ssl" | "tls" => true,
        other => {
            return Err(format!(
                "不支持的 broker 协议：{other}（支持 mqtt、mqtts、tcp、tls）。"
            ))
        }
    };
    let default_port = if tls { 8883 } else { 1883 };
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|value| value.is_ascii_digit()) && !port.is_empty() => {
            let port = port
                .parse::<u16>()
                .map_err(|_| format!("broker 端口不合法：{port}"))?;
            (host.to_string(), port)
        }
        _ => (rest.to_string(), default_port),
    };
    if host.is_empty() {
        return Err(format!("broker 地址缺少主机名：{trimmed}"));
    }
    Ok(BrokerAddress { host, port, tls })
}

/// 连接传输：明文或 TLS。TLS 变体额外持有一个克隆的 socket 句柄，只用来改读超时
/// （套接字选项作用于同一个内核对象，克隆句柄改完对 stream 也生效）。
enum Transport {
    Plain(TcpStream),
    Tls {
        stream: Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>,
        socket: TcpStream,
    },
}

impl Transport {
    fn set_read_timeout(&self, timeout: Duration) -> io::Result<()> {
        match self {
            Transport::Plain(socket) => socket.set_read_timeout(Some(timeout)),
            Transport::Tls { socket, .. } => socket.set_read_timeout(Some(timeout)),
        }
    }
}

impl Read for Transport {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Transport::Plain(socket) => socket.read(buffer),
            Transport::Tls { stream, .. } => stream.read(buffer),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Transport::Plain(socket) => socket.write(buffer),
            Transport::Tls { stream, .. } => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Transport::Plain(socket) => socket.flush(),
            Transport::Tls { stream, .. } => stream.flush(),
        }
    }
}

/// 一个已连接的 MQTT 会话。
pub struct MqttClient {
    transport: Transport,
    client_id: String,
    keepalive: Duration,
    last_write: Instant,
    ping_sent_at: Option<Instant>,
    next_packet_id: u16,
}

impl MqttClient {
    /// 建连并完成 CONNECT/CONNACK 握手。返回值非 0 一律按错误上报。
    pub fn connect(config: &MqttConfig) -> Result<Self, String> {
        let address = parse_broker_url(&config.broker)?;
        let target = (address.host.as_str(), address.port);
        let endpoints = target
            .to_socket_addrs()
            .map_err(|error| format!("解析 broker 地址失败（{}）：{error}", config.broker))?
            .collect::<Vec<_>>();
        let Some(endpoint) = endpoints.first().copied() else {
            return Err(format!("broker 地址没有可用端点：{}", config.broker));
        };
        let tcp = TcpStream::connect(endpoint)
            .map_err(|error| format!("连接 broker 失败（{}）：{error}", config.broker))?;
        let _ = tcp.set_nodelay(true);

        let transport = if address.tls {
            let socket = tcp
                .try_clone()
                .map_err(|error| format!("复制 socket 句柄失败：{error}"))?;
            Transport::Tls {
                stream: Box::new(tls_stream(tcp, &address.host)?),
                socket,
            }
        } else {
            Transport::Plain(tcp)
        };

        let mut client = Self {
            transport,
            client_id: config.client_id.clone(),
            keepalive: Duration::from_secs(config.keepalive_seconds.max(5)),
            last_write: Instant::now(),
            ping_sent_at: None,
            next_packet_id: 1,
        };
        client.transport.set_read_timeout(READ_SLICE).ok();

        let password = config.resolve_password();
        let connect = codec::connect_packet(
            &client.client_id,
            &config.username,
            &password,
            config.keepalive_seconds.min(u16::MAX as u64) as u16,
            true,
        );
        client.write_all(&connect)?;
        match client.read_packet() {
            Ok(Packet::ConnAck { return_code, .. }) if return_code == 0 => Ok(client),
            Ok(Packet::ConnAck { return_code, .. }) => Err(format!(
                "broker 拒绝连接（{}）：{return_code}",
                codec::connack_reason(return_code)
            )),
            Ok(other) => Err(format!("握手未收到 CONNACK，收到：{other:?}")),
            Err(error) => Err(format!("等待 CONNACK 失败：{error}")),
        }
    }

    /// 订阅一组主题；任一 SUBACK 返回码为 0x80 都按失败处理。
    pub fn subscribe(&mut self, topics: &[String], qos: u8) -> Result<(), String> {
        if topics.is_empty() {
            return Ok(());
        }
        let packet_id = self.take_packet_id();
        let packet = codec::subscribe_packet(packet_id, topics, qos);
        self.write_all(&packet)?;
        match self.read_packet() {
            Ok(Packet::SubAck { codes, .. }) => {
                if let Some(code) = codes.iter().find(|code| **code == 0x80) {
                    return Err(format!("broker 拒绝了订阅（返回码 {code}）：{topics:?}"));
                }
                Ok(())
            }
            Ok(other) => Err(format!("订阅未收到 SUBACK，收到：{other:?}")),
            Err(error) => Err(format!("等待 SUBACK 失败：{error}")),
        }
    }

    /// 确认一条入站 QoS1 发布：不确认 broker 会按重传策略反复投递。
    pub fn ack_publish(&mut self, packet_id: u16) -> Result<(), String> {
        self.write_all(&codec::puback_packet(packet_id))
    }

    /// 发布一条报文。QoS1 的 PUBACK 不在这里等：真正的执行结果以设备回执主题为准。
    pub fn publish(&mut self, topic: &str, payload: &[u8], qos: u8) -> Result<u16, String> {
        let packet_id = self.take_packet_id();
        let packet = codec::publish_packet(topic, payload, qos, Some(packet_id), false);
        self.write_all(&packet)?;
        Ok(packet_id)
    }

    /// 读一个报文；读超时返回 Ok(None)，表示本轮没有事件。
    ///
    /// 超时值由调用方给：主循环用 poll_seconds（一轮多久回到窗口与心跳检查），
    /// 等回执时用小切片，免得回执被拖到下一个整秒。
    pub fn poll(&mut self, timeout: Duration) -> Result<Option<Packet>, String> {
        self.transport.set_read_timeout(timeout.max(Duration::from_millis(50))).ok();
        match codec::read_packet(&mut self.transport) {
            Ok(packet) => {
                if let Packet::PingResp = packet {
                    self.ping_sent_at = None;
                }
                Ok(Some(packet))
            }
            Err(error) if is_timeout(&error) => Ok(None),
            Err(error) => Err(format!("读取 broker 报文失败：{error}")),
        }
    }

    /// 到了心跳点没有：按 keepalive 的一半发 PINGREQ。
    pub fn keepalive_due(&self) -> bool {
        if self.ping_sent_at.is_some() {
            return false;
        }
        self.last_write.elapsed() >= self.keepalive / 2
    }

    /// 发 PINGREQ；上一个 PINGRESP 迟迟不回就按断线处理。
    pub fn ping(&mut self) -> Result<(), String> {
        if let Some(sent_at) = self.ping_sent_at {
            if sent_at.elapsed() > self.keepalive {
                return Err("心跳无响应：broker 可能已断开。".to_string());
            }
            return Ok(());
        }
        self.write_all(&codec::pingreq_packet())?;
        self.ping_sent_at = Some(Instant::now());
        Ok(())
    }

    /// 正常断开：发 DISCONNECT（发不出去也不影响退出）。
    pub fn disconnect(&mut self) {
        let _ = self.write_all(&codec::disconnect_packet());
    }

    fn take_packet_id(&mut self) -> u16 {
        let id = self.next_packet_id;
        self.next_packet_id = if self.next_packet_id == u16::MAX {
            1
        } else {
            self.next_packet_id + 1
        };
        id
    }

    fn read_packet(&mut self) -> io::Result<Packet> {
        codec::read_packet(&mut self.transport)
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.transport
            .write_all(bytes)
            .and_then(|_| self.transport.flush())
            .map_err(|error| format!("写入 broker 失败：{error}"))?;
        self.last_write = Instant::now();
        Ok(())
    }
}

/// 建 TLS 流：与 ws.rs 的握手前置一致（系统根证书集 + 无客户端证书）。
fn tls_stream(
    tcp: TcpStream,
    host: &str,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name =
        rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|error| error.to_string())?;
    let connection = rustls::ClientConnection::new(std::sync::Arc::new(tls_config), server_name)
        .map_err(|error| error.to_string())?;
    Ok(rustls::StreamOwned::new(connection, tcp))
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_four_schemes() {
        assert_eq!(
            parse_broker_url("mqtt://10.0.0.5:1883").unwrap(),
            BrokerAddress {
                host: "10.0.0.5".to_string(),
                port: 1883,
                tls: false
            }
        );
        assert_eq!(
            parse_broker_url("mqtts://farm.example.com").unwrap(),
            BrokerAddress {
                host: "farm.example.com".to_string(),
                port: 8883,
                tls: true
            }
        );
        assert_eq!(
            parse_broker_url("tcp://localhost:1884").unwrap().port,
            1884
        );
        assert!(parse_broker_url("tls://broker.internal:8883").unwrap().tls);
    }

    #[test]
    fn rejects_unknown_scheme_and_empty_host() {
        assert!(parse_broker_url("http://localhost:1883").is_err());
        assert!(parse_broker_url("mqtt://").is_err());
        assert!(parse_broker_url("localhost:1883").is_err());
    }
}
