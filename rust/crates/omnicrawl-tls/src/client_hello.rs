//! ClientHello 的构造：扩展顺序、GREASE、密码套件顺序完全由档案决定。
//!
//! 这是整条路线里唯一无法用现成库替代的部分。`rustls` 的扩展顺序写死在
//! `emit_extensions` 里，也没有 GREASE 支持；而 JA3/JA4 的主要指纹字段正是
//! 「扩展出现的顺序」与「密码套件的原始排列」。所以这一层自己写，其余（记录层、
//! 密钥调度、证书校验）尽量复用已审计的实现。
//!
//! 刻意不做的事：不重排、不去重、不补齐「看起来缺失」的扩展。档案给什么就写什么，
//! 少一个扩展只是指纹不同，擅自补一个则会让指纹变成谁都不认识的样子。

use crate::wire::{
    handshake_message, push_extensions, push_prefixed, push_u16, push_u16_list, EncodeError,
    Extension, LengthWidth,
};

/// TLS 扩展类型编号。
pub mod ext {
    pub const SERVER_NAME: u16 = 0x0000;
    pub const STATUS_REQUEST: u16 = 0x0005;
    pub const SUPPORTED_GROUPS: u16 = 0x000a;
    pub const EC_POINT_FORMATS: u16 = 0x000b;
    pub const SIGNATURE_ALGORITHMS: u16 = 0x000d;
    pub const ALPN: u16 = 0x0010;
    pub const SIGNED_CERTIFICATE_TIMESTAMP: u16 = 0x0012;
    pub const PADDING: u16 = 0x0015;
    pub const EXTENDED_MASTER_SECRET: u16 = 0x0017;
    pub const COMPRESS_CERTIFICATE: u16 = 0x001b;
    pub const SESSION_TICKET: u16 = 0x0023;
    pub const PRE_SHARED_KEY: u16 = 0x0029;
    pub const SUPPORTED_VERSIONS: u16 = 0x002b;
    pub const COOKIE: u16 = 0x002c;
    pub const PSK_KEY_EXCHANGE_MODES: u16 = 0x002d;
    pub const KEY_SHARE: u16 = 0x0033;
    pub const APPLICATION_SETTINGS: u16 = 0x4469;
    pub const RENEGOTIATION_INFO: u16 = 0xff01;
}

/// 命名组编号（`supported_groups` 与 `key_share` 共用）。
pub mod group {
    pub const SECP256R1: u16 = 0x0017;
    pub const SECP384R1: u16 = 0x0018;
    pub const SECP521R1: u16 = 0x0019;
    pub const X25519: u16 = 0x001d;
    pub const X25519_MLKEM768: u16 = 0x11ec;
}

/// 密码套件编号（只列本项目档案用到的）。
pub mod suite {
    pub const TLS_AES_128_GCM_SHA256: u16 = 0x1301;
    pub const TLS_AES_256_GCM_SHA384: u16 = 0x1302;
    pub const TLS_CHACHA20_POLY1305_SHA256: u16 = 0x1303;
    pub const TLS_ECDHE_ECDSA_AES128_GCM: u16 = 0xc02b;
    pub const TLS_ECDHE_RSA_AES128_GCM: u16 = 0xc02f;
    pub const TLS_ECDHE_ECDSA_AES256_GCM: u16 = 0xc02c;
    pub const TLS_ECDHE_RSA_AES256_GCM: u16 = 0xc030;
    pub const TLS_ECDHE_ECDSA_CHACHA20: u16 = 0xcca9;
    pub const TLS_ECDHE_RSA_CHACHA20: u16 = 0xcca8;
}

/// GREASE 占位标记：序列化时替换成真正的 GREASE 值。
///
/// Chrome 在每个可容纳 GREASE 的列表里放一个，且同一次握手内取值一致。用标记而不是
/// 具体数值，是为了让档案与「这一次用到哪个值」解耦——值每次连接都要换。
pub const GREASE_MARK: u16 = 0xFEFE;

/// GREASE 的候选值（RFC 8701：`0x?A?A` 且两字节相同）。
pub const GREASE_VALUES: [u16; 16] = [
    0x0a0a, 0x1a1a, 0x2a2a, 0x3a3a, 0x4a4a, 0x5a5a, 0x6a6a, 0x7a7a, 0x8a8a, 0x9a9a, 0xaaaa, 0xbaba,
    0xcaca, 0xdada, 0xeaea, 0xfafa,
];

/// ClientHello 的一个扩展槽位。
///
/// `data` 里若出现 [`GREASE_MARK`]，序列化时按该次握手选定的 GREASE 值替换。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionSlot {
    pub kind: u16,
    pub data: Vec<u8>,
}

impl ExtensionSlot {
    pub fn new(kind: u16, data: Vec<u8>) -> Self {
        Self { kind, data }
    }

    /// 把槽位里的 GREASE 标记替换成具体值。
    ///
    /// 只按槽位的类型替换它自己的头部；数据段内的 GREASE 需要按各扩展的结构处理，
    /// 统一扫描会把长度前缀误会成值（`supported_versions` 的首字节就是长度）。
    fn materialize(&self, grease: u16) -> Extension {
        let kind = if self.kind == GREASE_MARK {
            grease
        } else {
            self.kind
        };
        let data = match self.kind {
            ext::SUPPORTED_GROUPS | ext::SIGNATURE_ALGORITHMS | ext::KEY_SHARE => {
                let mut data = self.data.clone();
                replace_grease_in_u16_lists(&mut data, self.kind, grease);
                data
            }
            ext::SUPPORTED_VERSIONS => {
                // 形状：1 字节长度 + 若干 16 位版本。
                let mut data = self.data.clone();
                if !data.is_empty() {
                    let end = (1 + data[0] as usize).min(data.len());
                    let mut index = 1;
                    while index + 1 < end {
                        if data[index] == 0xfe && data[index + 1] == 0xfe {
                            data[index..index + 2].copy_from_slice(&grease.to_be_bytes());
                        }
                        index += 2;
                    }
                }
                data
            }
            _ => self.data.clone(),
        };
        Extension::new(kind, data)
    }
}

/// 在「2 字节长度 + 值列表」形状的数据段里替换 GREASE 标记。
///
/// `supported_groups` 与 `signature_algorithms` 是纯值列表，`key_share` 的每个条目
/// 带自己的公钥长度，因此推进步长不同。GREASE 标记统一是 `0xFEFE`。
fn replace_grease_in_u16_lists(data: &mut [u8], kind: u16, grease: u16) {
    if data.len() < 2 {
        return;
    }
    let declared = u16::from_be_bytes([data[0], data[1]]) as usize;
    let end = (2 + declared).min(data.len());
    let marker = GREASE_MARK.to_be_bytes();
    let mut index = 2;
    while index + 1 < end {
        if data[index] == marker[0] && data[index + 1] == marker[1] {
            data[index..index + 2].copy_from_slice(&grease.to_be_bytes());
        }
        if kind == ext::KEY_SHARE {
            if index + 3 >= end {
                break;
            }
            let key_len = u16::from_be_bytes([data[index + 2], data[index + 3]]) as usize;
            index += 4 + key_len;
        } else {
            index += 2;
        }
    }
}

/// 一份完整的 ClientHello 蓝图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHelloSpec {
    /// `legacy_version`。TLS 1.3 仍写 0x0303，真实版本在 `supported_versions` 里。
    pub legacy_version: u16,
    /// 会话 ID 长度（字节数，内容随机）。
    pub session_id_len: usize,
    /// 密码套件，顺序即线上顺序。
    pub cipher_suites: Vec<u16>,
    /// 压缩方法，浏览器固定只发 `0x00`（null）。
    pub compression_methods: Vec<u8>,
    /// 扩展槽位，顺序即线上顺序。
    pub extensions: Vec<ExtensionSlot>,
}

impl ClientHelloSpec {
    /// 序列化为完整的 ClientHello 握手消息（含类型与三字节长度）。
    ///
    /// `grease` 是本次握手使用的 GREASE 值，由 [`pick_grease`] 按种子选取；
    /// `random` 与 `session_id` 由调用方给，保持这一层无随机源、可复现。
    ///
    /// `session_id` 的长度必须与 [`Self::session_id_len`] 一致：文件里声明的长度与
    /// 实际写入的长度若不符，握手整体会错位，且错位点远离出错原因。这里直接报错。
    pub fn encode(
        &self,
        grease: u16,
        random: &[u8; 32],
        session_id: &[u8],
    ) -> Result<Vec<u8>, EncodeError> {
        if session_id.len() != self.session_id_len {
            return Err(EncodeError {
                what: "会话 ID 长度与档案声明不符",
                len: session_id.len(),
                max: self.session_id_len,
            });
        }
        let mut body = Vec::new();
        push_u16(&mut body, self.legacy_version);
        body.extend_from_slice(random);
        push_prefixed(&mut body, LengthWidth::U8, "会话 ID", session_id)?;

        let mut suites = Vec::with_capacity(self.cipher_suites.len() * 2);
        for value in &self.cipher_suites {
            let value = if *value == GREASE_MARK {
                grease
            } else {
                *value
            };
            push_u16(&mut suites, value);
        }
        push_prefixed(&mut body, LengthWidth::U16, "密码套件", &suites)?;
        push_prefixed(
            &mut body,
            LengthWidth::U8,
            "压缩方法",
            &self.compression_methods,
        )?;

        // 每个 GREASE 位置必须取**不同**的值（RFC 8701 §3：同一连接里不同位置
        // 使用相同的 GREASE 值是协议错误）。真实 Chrome 的首尾两个 GREASE 扩展
        // 就是两个不同的值（抓包实测 0xbaba 与 0x7a7a）。
        //
        // 实测后果：两个 GREASE 扩展用同一个值发往 example.com 会被 alert
        // decode_error(50) 直接拒掉；只留一个则正常。
        let base = GREASE_VALUES
            .iter()
            .position(|value| *value == grease)
            .unwrap_or(0);
        let mut slot_index = 0usize;
        let extensions: Vec<Extension> = self
            .extensions
            .iter()
            .map(|slot| {
                let value = if slot.kind == GREASE_MARK {
                    // 相邻两个位置取 GREASE_VALUES 里相隔 8 的项，保证不同。
                    let picked = GREASE_VALUES[(base + slot_index * 8) % GREASE_VALUES.len()];
                    slot_index += 1;
                    picked
                } else {
                    grease
                };
                slot.materialize(value)
            })
            .collect();
        push_extensions(&mut body, &extensions)?;
        handshake_message(0x01, &body)
    }

    /// 按档案声明的长度生成会话 ID 占位（内容由调用方的随机源决定）。
    ///
    /// Safari 声明 0 字节、Chromium 系声明 32 字节；调用方不必记这些差异。
    pub fn empty_session_id(&self) -> Vec<u8> {
        vec![0u8; self.session_id_len]
    }

    /// 本档案的 JA3 指纹字符串（不含末尾的 MD5）。
    ///
    /// 用于与真实浏览器对照：JA3 的字段依次是版本、密码套件、扩展、椭圆曲线、
    /// 曲线点格式。GREASE 不参与 JA3 计算（真实浏览器发出的 GREASE 值会被
    /// 指纹系统忽略），因此这里原样输出标记值，对照时应剔除。
    pub fn ja3_string(&self) -> String {
        let version = self.legacy_version;
        let ciphers = self
            .cipher_suites
            .iter()
            .filter(|value| **value != GREASE_MARK)
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("-");
        let extensions = self
            .extensions
            .iter()
            .filter(|slot| slot.kind != GREASE_MARK)
            .map(|slot| slot.kind.to_string())
            .collect::<Vec<_>>()
            .join("-");
        let groups = self
            .extension_data(ext::SUPPORTED_GROUPS)
            .map(grep_u16_list)
            .unwrap_or_default()
            .into_iter()
            .filter(|value| *value != GREASE_MARK)
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("-");
        let point_formats = self
            .extension_data(ext::EC_POINT_FORMATS)
            .map(|data| {
                // 首字节是列表长度，其后是各格式值。
                data.iter()
                    .skip(1)
                    .map(|value| value.to_string())
                    .collect::<Vec<_>>()
                    .join("-")
            })
            .unwrap_or_default();
        format!("{version},{ciphers},{extensions},{groups},{point_formats}")
    }

    /// 带 PSK 的编码：把 `pre_shared_key` 追加为**最后一个**扩展，并返回 binder 的写入位置。
    ///
    /// RFC 8446 §4.2.11：`pre_shared_key` 必须是 ClientHello 的最后一项扩展——服务端据此
    /// 定位 binder，放错位置会被直接拒绝。
    ///
    /// 返回 `(完整 ClientHello, binder 起始偏移, binder 长度)`。计算 binder 时必须用
    /// 「截断到 binder 起始偏移」的前缀，且长度字段按 binder 已存在来计算——这正是
    /// 返回完整字节 + 偏移的意义：长度字段已经写好了，只截字节。
    pub fn encode_with_psk(
        &self,
        grease: u16,
        random: &[u8; 32],
        session_id: &[u8],
        identities: &[(Vec<u8>, u32)],
        binder_len: usize,
    ) -> Result<(Vec<u8>, usize, usize), EncodeError> {
        // 先按原样编码，再把 pre_shared_key 插到扩展块末尾并修正两层长度。
        let base = self.encode(grease, random, session_id)?;
        if identities.is_empty() {
            return Err(EncodeError {
                what: "PSK identities 不能为空",
                len: 0,
                max: usize::MAX,
            });
        }

        // 计算 pre_shared_key 的数据段：identities 列表 + binders 列表。
        let mut identity_list = Vec::new();
        for (identity, obfuscated_age) in identities {
            push_prefixed(&mut identity_list, LengthWidth::U16, "票据", identity)?;
            identity_list.extend_from_slice(&obfuscated_age.to_be_bytes());
        }
        let mut psk_data = Vec::new();
        push_prefixed(&mut psk_data, LengthWidth::U16, "identities", &identity_list)?;
        // 每个 binder 是 PskBinderEntry：1 字节长度 + 实际 binder。漏掉那个长度字节会让
        // binders 列表长度少 1，服务端解析到 binder 之前就报 decode_error(50)。
        let mut binder_entries = Vec::with_capacity(binder_len + 1);
        binder_entries.push(binder_len as u8);
        binder_entries.extend_from_slice(&vec![0u8; binder_len]);
        let mut binders = Vec::new();
        push_prefixed(&mut binders, LengthWidth::U16, "binders", &binder_entries)?;
        psk_data.extend_from_slice(&binders);

        // 完整扩展：类型(2) + 长度(2) + 数据。
        let mut extension = Vec::new();
        push_u16(&mut extension, ext::PRE_SHARED_KEY);
        push_prefixed(&mut extension, LengthWidth::U16, "pre_shared_key", &psk_data)?;

        // 把扩展追加进 ClientHello 的扩展块，同时修正两层长度。
        //
        // 不在这里手工推算偏移：ClientHello 的 body 由「固定头部（版本/随机数/会话 ID/
        // 套件/压缩方法）+ 扩展块」组成，扩展块前缀的长度字段位置取决于前几段的可变长度。
        // 手工算错一字节不会报错，只会把长度写到相邻字段上（实测差值正好是 256 的倍数）。
        // 因此改为先**解析**定位扩展块，再重建 body。
        let body = &base[4..];
        let mut cursor = 0usize;
        cursor += 2; // legacy_version
        cursor += 32; // random
        let session_id_len = body[cursor] as usize;
        cursor += 1 + session_id_len;
        let suites_len = u16::from_be_bytes([body[cursor], body[cursor + 1]]) as usize;
        cursor += 2 + suites_len;
        let compression_len = body[cursor] as usize;
        cursor += 1 + compression_len;
        // 到这里是扩展块长度字段的位置。
        let extension_len_offset = cursor;
        let declared =
            u16::from_be_bytes([body[extension_len_offset], body[extension_len_offset + 1]])
                as usize;
        let extensions_start = extension_len_offset + 2;
        debug_assert_eq!(
            extensions_start + declared,
            body.len(),
            "扩展块长度必须与 body 自洽"
        );

        let mut extensions = body[extensions_start..].to_vec();
        // 截断点：只截掉 binder 数据本身，**保留** binders 列表长度前缀（2 字节）与
        // 每个 entry 的 1 字节长度。依据是 RFC 8448 §4：那里 `payload` 恰好在
        // `ClientHello prefix` 之后只多出 32 字节（binder 数据），其余长度字段都在 prefix 内。
        // 多截这 3 字节会让服务端算出不同的 binder，直接 alert decrypt_error(51)。
        let binder_data_start_in_extensions =
            extensions.len() + extension.len() - binder_len;
        // 返回给调用方的偏移是 binder 数据起点（回填用）；截断点另由 `binder_offset - 3` 推出。
        let binder_start_in_extensions = binder_data_start_in_extensions;
        extensions.extend_from_slice(&extension);

        let mut body = body[..extension_len_offset].to_vec();
        push_prefixed(&mut body, LengthWidth::U16, "扩展块", &extensions)?;

        let mut hello = Vec::with_capacity(4 + body.len());
        hello.extend_from_slice(&base[..4]);
        hello.extend_from_slice(&body);
        // 握手消息头里的 3 字节 body 长度也要更新。
        let body_len = hello.len() - 4;
        hello[1] = ((body_len >> 16) & 0xff) as u8;
        hello[2] = ((body_len >> 8) & 0xff) as u8;
        hello[3] = (body_len & 0xff) as u8;

        let binder_offset = 4 + extension_len_offset + 2 + binder_start_in_extensions;
        Ok((hello, binder_offset, binder_len))
    }

    fn extension_data(&self, kind: u16) -> Option<&[u8]> {
        self.extensions
            .iter()
            .find(|slot| slot.kind == kind)
            .map(|slot| slot.data.as_slice())
    }
}

/// 从两字节长度前缀的列表体里读出全部 16 位值。
///
/// `supported_groups` 与 `signature_algorithms` 的数据段都是「`uint16` 长度 + 值列表」，
/// 长度覆盖的是值本身、不含这两个字节。跳过 3 字节（2 字节长度 + 前缀自身）都不对，
/// 正确做法是按声明长度切出值区。
fn grep_u16_list(data: &[u8]) -> Vec<u16> {
    if data.len() < 2 {
        return Vec::new();
    }
    let declared = u16::from_be_bytes([data[0], data[1]]) as usize;
    let end = (2 + declared).min(data.len());
    data[2..end]
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect()
}

/// 按种子选一个 GREASE 值；同一次握手的各处共用它。
pub fn pick_grease(seed: u8) -> u16 {
    GREASE_VALUES[(seed as usize) % GREASE_VALUES.len()]
}

/// `server_name` 扩展的数据段。
pub fn server_name(name: &str) -> Vec<u8> {
    let mut inner = Vec::new();
    inner.push(0); // name_type = host_name
    let _ = push_prefixed(&mut inner, LengthWidth::U16, "主机名", name.as_bytes());
    let mut out = Vec::new();
    let _ = push_prefixed(&mut out, LengthWidth::U16, "SNI 列表", &inner);
    out
}

/// `supported_groups` 扩展的数据段。
pub fn supported_groups(groups: &[u16]) -> Vec<u8> {
    let mut out = Vec::new();
    let _ = push_u16_list(&mut out, groups);
    out
}

/// `ec_point_formats` 扩展的数据段（浏览器固定 `uncompressed`）。
pub fn ec_point_formats() -> Vec<u8> {
    vec![0x01, 0x00]
}

/// `signature_algorithms` 扩展的数据段。
pub fn signature_algorithms(algorithms: &[u16]) -> Vec<u8> {
    let mut out = Vec::new();
    let _ = push_u16_list(&mut out, algorithms);
    out
}

/// `application_layer_protocol_negotiation` 扩展的数据段。
pub fn alpn(protocols: &[&str]) -> Vec<u8> {
    let mut list = Vec::new();
    for protocol in protocols {
        let _ = push_prefixed(&mut list, LengthWidth::U8, "ALPN 项", protocol.as_bytes());
    }
    let mut out = Vec::new();
    let _ = push_prefixed(&mut out, LengthWidth::U16, "ALPN 列表", &list);
    out
}

/// `supported_versions` 扩展的数据段（客户端发列表）。
pub fn supported_versions(versions: &[u16]) -> Vec<u8> {
    let mut list = Vec::new();
    for version in versions {
        push_u16(&mut list, *version);
    }
    let mut out = Vec::new();
    let _ = push_prefixed(&mut out, LengthWidth::U8, "版本列表", &list);
    out
}

/// `psk_key_exchange_modes` 扩展的数据段。
pub fn psk_key_exchange_modes(modes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let _ = push_prefixed(&mut out, LengthWidth::U8, "PSK 模式", modes);
    out
}

/// `key_share` 扩展的数据段：客户端预先给出各组的公钥。
pub fn key_share(entries: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut list = Vec::new();
    for (group, key) in entries {
        push_u16(&mut list, *group);
        let _ = push_prefixed(&mut list, LengthWidth::U16, "公钥", key);
    }
    let mut out = Vec::new();
    let _ = push_prefixed(&mut out, LengthWidth::U16, "key_share 列表", &list);
    out
}
