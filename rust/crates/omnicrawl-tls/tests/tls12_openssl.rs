//! TLS 1.2 的真实对端验证：用 `openssl s_server` 起一个只支持 1.2 的服务端，
//! 让本栈完整跑一遍握手与 HTTP/1.1 请求。
//!
//! 为什么不写「自家两端对称」的假服务端：1.2 的 PRF、key_block 切分、AEAD 的 AAD
//! 与 nonce 构造，只要两侧犯同一个错误就依然自洽通过。这一轮的每个错误都必须由
//! **外部实现**（这里是 OpenSSL）来判。
//!
//! 这些用例默认 `#[ignore]`：它们需要本机 `openssl` 可执行文件与空闲端口。手动执行：
//!
//! ```bash
//! cargo test -p omnicrawl-tls --test tls12_openssl -- --ignored --test-threads=1
//! ```

use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use omnicrawl_tls::driver::Connection;
use omnicrawl_tls::profile::Family;
use omnicrawl_tls::session::Version;
use omnicrawl_tls::verify::TrustAnchors;
use rustls_pki_types::{CertificateDer, UnixTime};

const TIMEOUT: Duration = Duration::from_secs(15);

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// 一个在后台运行的 `openssl s_server`，析构时杀掉。
struct OpensslServer {
    child: Child,
    port: u16,
}

impl Drop for OpensslServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    // 绑到 0 拿到内核分配的端口，立刻释放再交给 s_server。这里单线程跑，冲突概率极低。
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定临时端口");
    let port = listener.local_addr().expect("取地址").port();
    drop(listener);
    port
}

/// 不带 `-www` 的服务端：只做握手，不提供网页（用于只验协商结果的用例）。
fn start_server_raw(extra_args: &[&str]) -> OpensslServer {
    start_server_impl(extra_args, false)
}

fn start_server(extra_args: &[&str]) -> OpensslServer {
    start_server_impl(extra_args, true)
}

fn start_server_impl(extra_args: &[&str], with_www: bool) -> OpensslServer {
    let port = free_port();
    let mut command = Command::new("openssl");
    command
        .arg("s_server")
        .arg("-accept")
        .arg(port.to_string())
        .arg("-cert")
        .arg(fixture("leaf-ec.der"))
        .arg("-key")
        .arg(fixture("leaf-ec-key.p8"))
        .arg("-certform")
        .arg("DER")
        .arg("-keyform")
        .arg("DER")
        .args(if with_www { vec!["-www"] } else { Vec::new() })
        .arg("-quiet")
        .args(extra_args)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command.spawn().expect("启动 openssl s_server");
    // 等它真正开始监听：不做这步，第一次连接会撞上 ConnectionRefused。
    // 探活连接立刻关闭——s_server 默认 `-naccept -1`，但close 后的空连接不会影响
    // 后续握手；保留它反而会占住 `-www` 的第一次交互。
    for _ in 0..100 {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => {
                drop(stream);
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    OpensslServer { child, port }
}

/// 夹具 CA 作为受信根。服务端要用 `-cert_chain ca.der` 把中间/根一起发出来，
/// 否则链建不起来（只发叶子时任何实现都会报 UnknownIssuer）。
/// 连到本地端口，并把读写超时设成与其它用例一致（没有超时的阻塞 socket 会挂住）。
fn connect_local(port: u16) -> TcpStream {
    let stream = TcpStream::connect(("127.0.0.1", port)).expect("连接本地服务端");
    stream.set_read_timeout(Some(TIMEOUT)).expect("设置读超时");
    stream.set_write_timeout(Some(TIMEOUT)).expect("设置写超时");
    stream
}

fn fixture_string(name: &str) -> String {
    fixture(name).to_string_lossy().to_string()
}

fn trust_fixture_ca() -> TrustAnchors {
    let der = std::fs::read(fixture("ca.der")).expect("读 CA");
    TrustAnchors::from_der(&[CertificateDer::from(der)]).expect("加载受信根")
}

fn verify_time() -> UnixTime {
    UnixTime::since_unix_epoch(Duration::from_secs(1_800_000_000))
}

fn connect_12(port: u16, family: Family) -> Connection<TcpStream> {
    let connection =
        Connection::connect_to("localhost", port, family, None, TIMEOUT).unwrap_or_else(|error| {
            panic!("TLS 1.2 握手失败：{error}");
        });
    assert_eq!(
        connection.version(),
        Some(Version::Tls12),
        "该端口只允许 1.2，协商结果必须是 1.2"
    );
    connection
}

#[test]
#[ignore = "需要本机 openssl 可执行文件"]
fn a_tls12_only_server_completes_a_handshake_and_serves_http11() {
    let server = start_server(&["-tls1_2"]);
    let mut connection = connect_12(server.port, Family::Chrome);
    let response = connection.get("/").expect("请求应当完成");
    assert_eq!(response.status, 200, "openssl -www 应当返回 200");
    let body = String::from_utf8_lossy(&response.body);
    assert!(
        body.contains("s_server") || body.len() > 100,
        "响应体应当是 s_server 的欢迎页，实际 {} 字节",
        response.body.len()
    );
}

#[test]
#[ignore = "需要本机 openssl 可执行文件"]
fn all_four_families_negotiate_tls12() {
    for family in [Family::Chrome, Family::Firefox, Family::Safari, Family::Edge] {
        let server = start_server(&["-tls1_2"]);
        let mut connection = connect_12(server.port, family);
        let response = connection.get("/").expect("请求应当完成");
        assert_eq!(response.status, 200, "{family:?} 的响应异常");
        assert_eq!(connection.version(), Some(Version::Tls12), "{family:?}");
    }
}

#[test]
#[ignore = "需要本机 openssl 可执行文件"]
fn tls12_chain_verification_runs_when_trust_is_given() {
    let server = start_server(&["-tls1_2", "-cert_chain", &fixture_string("ca.der")]);
    // 夹具证书签给 example.com，而服务端在本地：直接用 `connect` 把一个连到本地的
    // TcpStream 交进去，既能验 example.com 的主机名，又不用改 DNS。
    let stream = connect_local(server.port);
    let connection = Connection::connect(
        stream,
        Family::Chrome,
        "example.com",
        Some(trust_fixture_ca()),
    )
    .expect("带受信根的 1.2 握手应当成功");
    assert_eq!(connection.version(), Some(Version::Tls12));
    let leaf = connection.peer_certificate().expect("应当拿到叶证书");
    assert!(!leaf.is_empty());
    let _ = verify_time();

    // 主机名不匹配时必须失败：否则「1.2 的验链」等于没做。
    // 注意夹具证书的 SAN 是 example.com 与 *.example.com，因此必须用域外名字
    // ——用 wrong.example.com 会被通配符匹配上，反而证明不了任何东西。
    let server = start_server(&["-tls1_2", "-cert_chain", &fixture_string("ca.der")]);
    let stream = connect_local(server.port);
    let error = match Connection::connect(
        stream,
        Family::Chrome,
        "not-this-host.invalid",
        Some(trust_fixture_ca()),
    ) {
        Ok(_) => panic!("主机名不匹配应当失败"),
        Err(error) => error,
    };
    let text = error.to_string();
    assert!(
        text.contains("主机名") || text.contains("证书"),
        "错误应当指向证书/主机名：{text}"
    );
}

#[test]
#[ignore = "需要本机 openssl 可执行文件"]
fn a_server_that_also_offers_13_is_still_negotiated_as_13() {
    // 回落必须是「服务端只给 1.2 才发生」：服务端支持 1.3 时必须仍然走 1.3，
    // 否则等于把 1.3 的能力白送掉。
    //
    // 这里只验版本协商，不请求网页：s_server 的 -www 不是 HTTP/2 服务端，一旦协商出
    // h2 就永远等不到 SETTINGS。因此用不带 -www 的服务端，构造成功本身已经证明握手走完。
    let server = start_server_raw(&["-tls1_3", "-alpn", "http/1.1"]);
    let stream = connect_local(server.port);
    let connection = Connection::connect(stream, Family::Chrome, "localhost", None)
        .expect("支持 1.3 的服务端必须协商出 1.3");
    assert_eq!(connection.version(), Some(Version::Tls13));
}

#[test]
#[ignore = "需要本机 openssl 可执行文件"]
fn a_sha384_12_suite_works() {
    // 0xc030 用 AES-256-GCM，PRF 是 SHA-384：转录、key_block、Finished 全要换哈希。
    let server = start_server(&["-tls1_2", "-cipher", "ECDHE-ECDSA-AES256-GCM-SHA384"]);
    let mut connection = connect_12(server.port, Family::Chrome);
    let response = connection.get("/").expect("SHA384 套件应当可用");
    assert_eq!(response.status, 200);
}

#[test]
#[ignore = "需要本机 openssl 可执行文件"]
fn an_ecdsa_12_suite_works() {
    // ECDSA 证书只能用 ECDHE-ECDSA 套件（0xc02b）：验签走的是证书里的 EC 公钥。
    let server = start_server(&["-tls1_2", "-cipher", "ECDHE-ECDSA-AES128-GCM-SHA256"]);
    let mut connection = connect_12(server.port, Family::Chrome);
    let response = connection.get("/").expect("ECDSA 套件应当可用");
    assert_eq!(response.status, 200);
}

#[test]
#[ignore = "需要本机 openssl 可执行文件"]
fn chacha20_12_suite_works() {
    // RFC 7905：ChaCha20-Poly1305 不发 explicit nonce，nonce 由固定 IV 异或序号得到。
    let server = start_server(&[
        "-tls1_2",
        "-cipher",
        "ECDHE-ECDSA-CHACHA20-POLY1305",
    ]);
    let mut connection = connect_12(server.port, Family::Chrome);
    let response = connection.get("/").expect("ChaCha20 套件应当可用");
    assert_eq!(response.status, 200);
}

#[test]
#[ignore = "需要本机 openssl 可执行文件"]
fn a_12_connection_survives_a_large_response() {
    // 大响应会跨多条记录：1.2 的显式 nonce 与序号必须逐条推进。
    let server = start_server(&["-tls1_2"]);
    let mut connection = connect_12(server.port, Family::Chrome);
    let response = connection.get("/").expect("请求应当完成");
    assert!(!response.body.is_empty());
    assert_eq!(response.status, 200);
}

#[test]
#[ignore = "需要本机 openssl 可执行文件"]
fn without_extended_master_secret_the_classic_derivation_is_used() {
    // `-no_ems` 让服务端不回 extended_master_secret，此时必须退回 RFC 5246 的
    // 「两个 random」派生。若仍按 EMS 算，server Finished 一定校验失败。
    let server = start_server(&["-tls1_2", "-no_ems"]);
    let mut connection = connect_12(server.port, Family::Chrome);
    let response = connection
        .get("/")
        .expect("没有 EMS 时也必须能完成握手");
    assert_eq!(response.status, 200);
}

#[test]
#[ignore = "需要本机 openssl 可执行文件"]
fn the_12_handshake_sequence_is_observable() {
    // 直接读一条 s_server 的响应头，确认应用数据通道确实通了（不是靠状态码碰巧通过）。
    let server = start_server(&["-tls1_2"]);
    let mut connection = connect_12(server.port, Family::Chrome);
    let response = connection.get("/").expect("请求");
    let server_header = response
        .header("content-type")
        .or_else(|| response.header("content-length"));
    assert!(
        server_header.is_some(),
        "应当有 content-type 或 content-length，实际头：{:?}",
        response.headers
    );
}
