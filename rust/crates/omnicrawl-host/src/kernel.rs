//! 内核进程客户端：起内核子进程，在它的 stdin/stdout 上收发协议 v1 的 NDJSON 帧。
//!
//! 读取放在独立线程里，主线程只在事件循环里 `try_recv` 取帧；写入由主线程独占，
//! 因此同一连接上「谁在什么时候写了哪一帧」是确定的。

use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use omnicrawl_config::core::bootstrap::default_api_key_env;
use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::models::llm::load_llm_config;
use omnicrawl_ipc::{error_code, ErrorObject, Frame, Id};

pub struct KernelClient {
    writer: Box<dyn Write + Send>,
    frames: Receiver<Frame>,
    child: Option<Child>,
    next_id: i64,
    closed: bool,
}

/// 内核子进程要补的环境变量（`(名字, 值)`）。
pub type KernelEnv = Vec<(String, String)>;

/// 凭据 → 内核子进程环境。
///
/// 协议帧里只带凭据的**变量名**（`KernelModelConfig.api_key_env`），内核的 `read_api_key`
/// 只读环境变量。而 `config.toml` 里写的字面 `api_key`（Python 侧的首选来源）并不在环境里，
/// 所以宿主必须在起内核时把它补进子进程环境，否则「配置里有 key、环境里没有」的用法会在
/// 每次回合上直接失败：`读取环境变量 OPENAI_API_KEY 失败…模型请求无法鉴权`。
/// （这是父子进程的凭据握手，不属于被移除的「用户设置类环境变量」。）
///
/// 密钥仍然只走进程环境，不进协议帧。名字为空时不注入（内核会按「无凭据」处理）。
pub fn model_credentials_env(api_key: &str, api_key_env: &str) -> KernelEnv {
    let key = api_key.trim();
    let name = api_key_env.trim();
    if key.is_empty() || name.is_empty() {
        return Vec::new();
    }
    vec![(name.to_string(), key.to_string())]
}

/// 从 `config.toml` 的主通道解析密钥，并按帧里的变量名补进内核环境（宿主两侧共用）。
///
/// `provider` / `api_key_env` 传宿主握手时交给内核的那两个值；变量名空白时用 Provider 默认名，
/// 与帧里保持一致（否则注入了内核也不会去读）。
pub fn kernel_credentials_env(
    env: &ConfigEnvironment,
    provider: &str,
    api_key_env: &str,
) -> KernelEnv {
    let name = frame_api_key_env(provider, api_key_env);
    let Ok(llm) = load_llm_config(env) else {
        return Vec::new();
    };
    model_credentials_env(&llm.api_key, &name)
}

/// 帧里该告诉内核的凭据变量名：给了就用，空白时退回 Provider 默认名。
pub fn frame_api_key_env(provider: &str, api_key_env: &str) -> String {
    let name = api_key_env.trim();
    if name.is_empty() {
        default_api_key_env(provider)
    } else {
        name.to_string()
    }
}

/// 内核 stderr 行的接收器：`spawn_with_stderr` 用它接住「会话已就绪」这类通知。
type StderrSink = Box<dyn Fn(&str) + Send>;

impl KernelClient {
    /// 起内核进程：stdout 交给读线程，stderr 直接继承（内核只在那里写日志）。
    pub fn spawn(program: &Path) -> io::Result<Self> {
        Self::spawn_with(program, None, Vec::new())
    }

    /// 起内核进程，并给子进程补一组环境变量（凭据注入，见 `model_credentials_env`）。
    pub fn spawn_with_env(program: &Path, envs: KernelEnv) -> io::Result<Self> {
        Self::spawn_with(program, None, envs)
    }

    /// 起内核进程，并把 stderr 逐行交给回调。
    ///
    /// 新会话的 ID **只经内核 stderr 报出**（协议 v1），需要它（例如本地 API 要
    /// 在后续回合复用同一会话）的宿主用这个入口接住 `[kernel] 会话已就绪：<id>`。
    pub fn spawn_with_stderr(
        program: &Path,
        sink: impl Fn(&str) + Send + 'static,
    ) -> io::Result<Self> {
        Self::spawn_with(program, Some(Box::new(sink)), Vec::new())
    }

    /// 起内核进程，同时补环境变量（凭据）并接管 stderr：本地 API 两者都要。
    pub fn spawn_with_stderr_env(
        program: &Path,
        envs: KernelEnv,
        sink: impl Fn(&str) + Send + 'static,
    ) -> io::Result<Self> {
        Self::spawn_with(program, Some(Box::new(sink)), envs)
    }

    fn spawn_with(
        program: &Path,
        sink: Option<StderrSink>,
        envs: KernelEnv,
    ) -> io::Result<Self> {
        let stderr = if sink.is_some() {
            Stdio::piped()
        } else {
            Stdio::inherit()
        };
        let mut command = Command::new(program);
        // 凭据只经环境交给内核（协议帧只带变量名）：见 `model_credentials_env`。
        for (name, value) in envs {
            command.env(name, value);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("无法启动内核进程 {}：{error}", program.display()),
                )
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("内核进程没有可读的 stdout"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("内核进程没有可写的 stdin"))?;
        if let Some(sink) = sink {
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| io::Error::other("内核进程没有可读的 stderr"))?;
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    sink(&line);
                }
            });
        }
        let mut client = Self::from_streams(Box::new(BufReader::new(stdout)), Box::new(stdin));
        client.child = Some(child);
        Ok(client)
    }

    /// 以任意一对输入输出流构造；回环与单测用它替代真子进程。
    pub fn from_streams(reader: Box<dyn BufRead + Send>, writer: Box<dyn Write + Send>) -> Self {
        let (sender, frames) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = reader;
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => match Frame::parse(&line) {
                        Ok(frame) => {
                            if sender.send(frame).is_err() {
                                break;
                            }
                        }
                        // 协议约定：非法行丢弃并记日志，不断开连接。
                        Err(error) => eprintln!("[tui] 丢弃内核发出的非法帧：{error}"),
                    },
                    Err(error) => {
                        eprintln!("[tui] 读取内核输出失败：{error}");
                        break;
                    }
                }
            }
        });
        Self {
            writer,
            frames,
            child: None,
            next_id: 1,
            closed: false,
        }
    }

    /// 取下一个已到达的帧；内核退出且缓冲耗尽后返回 `None` 并把连接标记为已关闭。
    pub fn try_recv(&mut self) -> Option<Frame> {
        match self.frames.try_recv() {
            Ok(frame) => Some(frame),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.closed = true;
                None
            }
        }
    }

    /// 等待一个帧，最多等 `timeout`；握手期用它拿 `initialize` 的响应。
    ///
    /// 注意它把「超时」与「连接断开」折叠成同一个 `None`：要区分二者就再看
    /// [`KernelClient::is_closed`]。
    pub fn recv_timeout(&mut self, timeout: Duration) -> Option<Frame> {
        self.frames.recv_timeout(timeout).ok()
    }

    /// 等到一个帧，或等到连接断开。
    ///
    /// 返回 `None` 后 `is_closed()` 一定是准的：断开这一支会把关闭状态置位，
    /// 超时这一支不会。长循环（回合驱动）用这个，免得把「暂时没数据」当成「内核没了」。
    pub fn recv_until_closed(&mut self, timeout: Duration) -> Option<Frame> {
        match self.frames.recv_timeout(timeout) {
            Ok(frame) => Some(frame),
            Err(RecvTimeoutError::Timeout) => match self.frames.try_recv() {
                Ok(frame) => Some(frame),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    self.closed = true;
                    None
                }
            },
            Err(RecvTimeoutError::Disconnected) => {
                self.closed = true;
                None
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn next_id(&mut self) -> Id {
        let id = self.next_id;
        self.next_id += 1;
        Id::Number(id)
    }

    pub fn send_frame(&mut self, frame: &Frame) -> io::Result<()> {
        let mut line = frame.to_line();
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()
    }

    /// 回一个成功响应。
    pub fn respond(&mut self, id: &Id, result: serde_json::Value) -> io::Result<()> {
        self.send_frame(&Frame::response(id.clone(), result))
    }

    /// 回一个失败响应；宿主不解的请求一律回 `-32601`，不让内核干等。
    pub fn respond_error(&mut self, id: &Id, code: i64, message: &str) -> io::Result<()> {
        self.send_frame(&Frame::error_response(
            id.clone(),
            ErrorObject::new(code, message),
        ))
    }

    /// 回「这个方法宿主没实现」。
    pub fn respond_unsupported(&mut self, id: &Id, method: &str) -> io::Result<()> {
        self.respond_error(
            id,
            error_code::METHOD_NOT_FOUND,
            &format!("宿主未实现协议方法 {method}。"),
        )
    }

    /// 收尾：等内核自己退出，超时没退就强杀，避免留下孤儿进程。
    pub fn wait_or_kill(&mut self, timeout: Duration) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    #[test]
    fn credentials_env_uses_the_frame_variable_name() {
        // 帧里给的是名字，注入的必须是同一个名字，否则内核不会去读。
        assert_eq!(
            model_credentials_env("sk-literal", "OPENAI_API_KEY"),
            vec![("OPENAI_API_KEY".to_string(), "sk-literal".to_string())]
        );
        assert_eq!(
            model_credentials_env(" sk ", " MY_KEY "),
            vec![("MY_KEY".to_string(), "sk".to_string())]
        );
    }

    #[test]
    fn credentials_env_skips_blank_key_or_name() {
        // 空密钥 / 空名字都不注入：内核自己会按「无凭据」处理。
        assert!(model_credentials_env("", "OPENAI_API_KEY").is_empty());
        assert!(model_credentials_env("   ", "OPENAI_API_KEY").is_empty());
        assert!(model_credentials_env("sk-literal", "").is_empty());
    }

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl SharedWriter {
        fn lines(&self) -> Vec<String> {
            let bytes = self.0.lock().expect("写入缓冲未被毒化").clone();
            String::from_utf8(bytes)
                .expect("帧是 UTF-8")
                .lines()
                .map(|line| line.to_string())
                .collect()
        }
    }

    impl Write for SharedWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("写入缓冲未被毒化")
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn client_with_script(script: &str) -> (KernelClient, SharedWriter) {
        let writer = SharedWriter::default();
        let client = KernelClient::from_streams(
            Box::new(BufReader::new(Cursor::new(script.as_bytes().to_vec()))),
            Box::new(writer.clone()),
        );
        (client, writer)
    }

    #[test]
    fn frames_are_parsed_and_bad_lines_are_skipped() {
        let script = concat!(
            "{\"jsonrpc\":\"2.0\",\"method\":\"turn.delta\",\"params\":{\"text\":\"hi\"}}\n",
            "这不是 JSON\n",
            "\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"turn.finished\",\"params\":{\"turn_id\":\"t1\"}}\n"
        );
        let (mut client, _) = client_with_script(script);
        let first = wait_for_frame(&mut client);
        assert_eq!(first.method(), Some("turn.delta"));
        let second = wait_for_frame(&mut client);
        assert_eq!(second.method(), Some("turn.finished"));
        // 流结束：连接标记为已关闭，且不再有新帧。
        assert!(client.try_recv().is_none());
        assert!(client.is_closed());
    }

    #[test]
    fn written_frames_are_single_line_ndjson() {
        let (mut client, writer) = client_with_script("");
        let id = client.next_id();
        client
            .send_frame(&Frame::request(
                id,
                omnicrawl_ipc::method::TURN_SUBMIT,
                serde_json::json!({"turn_id": "t1", "user_text": "第一行\n第二行"}),
            ))
            .expect("写帧应当成功");
        let lines = writer.lines();
        assert_eq!(lines.len(), 1, "一帧一行：{lines:?}");
        let frame = Frame::parse(&lines[0]).expect("写出的帧应当可解析");
        assert_eq!(frame.method(), Some("turn.submit"));
        assert_eq!(frame.id(), Some(&Id::Number(1)));
        assert!(
            lines[0].contains("第一行\\n第二行"),
            "换行必须被转义：{}",
            lines[0]
        );
    }

    #[test]
    fn responses_and_errors_are_written_as_responses() {
        let (mut client, writer) = client_with_script("");
        client
            .respond(&Id::Number(7), serde_json::json!({"ok": true}))
            .expect("回响应应当成功");
        client
            .respond_unsupported(&Id::Text("abc".to_string()), "model.reply")
            .expect("回错误应当成功");
        let lines = writer.lines();
        let success = Frame::parse(&lines[0]).expect("响应可解析");
        assert!(success.is_response());
        assert_eq!(success.id(), Some(&Id::Number(7)));
        let failure = Frame::parse(&lines[1]).expect("错误响应可解析");
        assert_eq!(failure.id(), Some(&Id::Text("abc".to_string())));
        assert_eq!(
            failure.error.as_ref().map(|error| error.code),
            Some(error_code::METHOD_NOT_FOUND)
        );
    }

    fn wait_for_frame(client: &mut KernelClient) -> Frame {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(frame) = client.try_recv() {
                return frame;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("等待内核帧超时");
    }
}
