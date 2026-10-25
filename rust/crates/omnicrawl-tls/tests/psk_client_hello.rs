//! PSK 扩展的结构验证：长度字段、binder 偏移、pre_shared_key 位置。
//!
//! 这些断言不用「看起来对」来确认，而是把编出来的 ClientHello **重新解析一遍**，
//! 检查两层长度（握手体、扩展块）与 binder 偏移是否自洽。长度字段写错的后果很隐蔽：
//! 不会报错，只会让服务端解析错位并拒绝握手。

use omnicrawl_tls::client_hello::ext;
use omnicrawl_tls::profile::{spec_for, Family};

fn new_hello() -> omnicrawl_tls::client_hello::ClientHelloSpec {
    // 只给一个 X25519 公钥：这一层的测试不关心密钥交换本身。
    spec_for(Family::Chrome, "example.com", &[(0x001d, vec![0x11; 32])])
}

/// 解析扩展块，返回 `(类型, 数据)` 列表；长度不自洽时返回 `None`。
fn parse_extensions(hello: &[u8]) -> Option<Vec<(u16, Vec<u8>)>> {
    // 握手消息头是 4 字节，之后是 body。
    let body_len = ((hello[1] as usize) << 16) | ((hello[2] as usize) << 8) | hello[3] as usize;
    assert_eq!(body_len, hello.len() - 4, "握手体长度必须与实际一致");

    let mut cursor = 4;
    cursor += 2; // legacy_version
    cursor += 32; // random
    let session_id_len = hello[cursor] as usize;
    cursor += 1 + session_id_len;
    let suites_len = u16::from_be_bytes([hello[cursor], hello[cursor + 1]]) as usize;
    cursor += 2 + suites_len;
    let compression_len = hello[cursor] as usize;
    cursor += 1 + compression_len;

    let ext_len = u16::from_be_bytes([hello[cursor], hello[cursor + 1]]) as usize;
    cursor += 2;
    if cursor + ext_len != hello.len() {
        return None;
    }
    let mut out = Vec::new();
    let end = cursor + ext_len;
    while cursor + 4 <= end {
        let kind = u16::from_be_bytes([hello[cursor], hello[cursor + 1]]);
        let len = u16::from_be_bytes([hello[cursor + 2], hello[cursor + 3]]) as usize;
        cursor += 4;
        if cursor + len > end {
            return None;
        }
        out.push((kind, hello[cursor..cursor + len].to_vec()));
        cursor += len;
    }
    if cursor != end {
        return None;
    }
    Some(out)
}

#[test]
fn psk_extension_is_appended_last_with_consistent_lengths() {
    let spec = new_hello();
    let random = [0x22u8; 32];
    let session_id = spec.empty_session_id();
    let binder_len = 32;
    let (hello, binder_offset, returned_len) = spec
        .encode_with_psk(
            0x0a0a,
            &random,
            &session_id,
            &[(b"ticket-abc".to_vec(), 1234)],
            binder_len,
        )
        .expect("编码");

    assert_eq!(returned_len, binder_len);
    let extensions = parse_extensions(&hello).expect("两层长度必须自洽");
    let last = extensions.last().expect("必须至少有一个扩展");
    assert_eq!(
        last.0,
        ext::PRE_SHARED_KEY,
        "pre_shared_key 必须是最后一个扩展"
    );

    // `binder_offset` 指向 binders 列表里第一个 PskBinderEntry 的**数据**起点；
    // 它前面依次是 1 字节 entry 长度与 2 字节 binders 列表长度（RFC 8446 §4.2.11）。
    // 因此截断点（Truncate，去掉整个 binders 列表）在它往回 3 字节处。
    assert!(binder_offset > 3);
    let declared_binders_len =
        u16::from_be_bytes([hello[binder_offset - 3], hello[binder_offset - 2]]) as usize;
    assert_eq!(
        declared_binders_len,
        binder_len + 1,
        "binders 列表长度字段 = entry 长度字节 + binder 数据"
    );
    assert_eq!(
        hello[binder_offset - 1] as usize,
        binder_len,
        "PskBinderEntry 的长度字节"
    );
    let binder = &hello[binder_offset..binder_offset + binder_len];
    assert_eq!(binder, &vec![0u8; binder_len], "编码阶段 binder 应为占位零");
}

/// 计算 binder 用的前缀必须是「截断到 binder 起始」，且长度字段已按 binder 存在写好。
#[test]
fn the_truncated_prefix_keeps_the_final_lengths() {
    let spec = new_hello();
    let random = [0x33u8; 32];
    let session_id = spec.empty_session_id();
    let (hello, binder_offset, binder_len) = spec
        .encode_with_psk(0x0a0a, &random, &session_id, &[(b"ticket".to_vec(), 0)], 32)
        .expect("编码");

    let truncated = &hello[..binder_offset - 3];
    // RFC 8446 §4.2.11.2：转录「一直到并包括 identities 字段」——`Truncate()` 去掉整个
    // binders 列表，含 2 字节列表长度前缀、1 字节 entry 长度与 binder 数据。
    assert_eq!(
        truncated.len(),
        hello.len() - binder_len - 3,
        "截断应去掉 binders 列表长度前缀 + entry 长度 + binder 数据"
    );
    // 截断后的握手体长度字段仍是「完整」的值——这正是规范要求的形式。
    let declared = ((hello[1] as usize) << 16) | ((hello[2] as usize) << 8) | hello[3] as usize;
    assert_eq!(declared, hello.len() - 4, "长度字段按完整 CH 声明");
    assert!(truncated.len() < 4 + declared, "截断后应当比声明的体长度短");
}

/// 空 identities 应当被拒绝：没有票据就没有可恢复的东西。
#[test]
fn empty_identities_are_rejected() {
    let spec = new_hello();
    let error = spec.encode_with_psk(0x0a0a, &[0u8; 32], &spec.empty_session_id(), &[], 32);
    assert!(error.is_err(), "空 identities 不该编出合法 CH");
}

/// 插了 PSK 之后，原有扩展必须原样保留（不能丢扩展，也不能改顺序）。
#[test]
fn the_original_extensions_survive_the_insertion() {
    let spec = new_hello();
    let (plain, _, _) = {
        let encoded = spec
            .encode(0x0a0a, &[0u8; 32], &spec.empty_session_id())
            .expect("基础编码");
        (encoded, 0usize, 0usize)
    };
    let (with_psk, _, _) = spec
        .encode_with_psk(
            0x0a0a,
            &[0u8; 32],
            &spec.empty_session_id(),
            &[(b"t".to_vec(), 0)],
            32,
        )
        .expect("PSK 编码");

    let before = parse_extensions(&plain).expect("基础 CH 可解析");
    let after = parse_extensions(&with_psk).expect("PSK CH 可解析");
    assert_eq!(
        after.len(),
        before.len() + 1,
        "只应多出 pre_shared_key 一项"
    );
    for (index, (kind, data)) in before.iter().enumerate() {
        assert_eq!(&after[index].0, kind, "第 {index} 个扩展的类型被改动");
        assert_eq!(&after[index].1, data, "第 {index} 个扩展的数据被改动");
    }
}
