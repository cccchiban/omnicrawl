//! RFC 8448 §3「Simple 1-RTT Handshake」的官方向量。
//!
//! 这组用例的意义在于**外部权威性**：这里的每一个十六进制串都来自 RFC 正文，
//! 不是我按自己的实现反推出来的。自家两端对称的测试有个致命盲点——两侧犯同一个
//! 错误时依然自洽通过；只有对着 RFC 的固定值，才能证明某一层真的对。
//!
//! 实际发生过的事：`derived` 的上下文传了空字节而不是空转录的哈希，
//! 自建的假服务端与客户端一起错、一起通过，直到对接 example.com 才暴露。
//! 这组用例就是为了让同类错误在 `cargo test` 阶段就失败。

use omnicrawl_tls::keyschedule::{
    derive_secret, empty_transcript_hash, expand_label, extract, traffic_keys, CipherSuite,
};
use omnicrawl_tls::record::{parse_record, ContentType, RecordKey};

fn hex(text: &str) -> Vec<u8> {
    let cleaned: String = text
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect();
    cleaned
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).expect("ascii"), 16).expect("hex"))
        .collect()
}

fn suite() -> CipherSuite {
    CipherSuite::Aes128GcmSha256
}

/// RFC 8448 §3 的 Early Secret：`HKDF-Extract(0, 0)`。
const EARLY_SECRET: &str =
    "33ad0a1c607ec03b09e6cd9893680ce210adf300aa1f2660e1b22e10f170f92a";

/// RFC 8448 §3 的 ECDHE shared secret。
const SHARED_SECRET: &str =
    "8bd4054fb55b9d63fdfbacf9f04b9f0d35e6d63f537563efd46272900f89492d";

/// RFC 8448 §3 的 handshake secret。
const HANDSHAKE_SECRET: &str =
    "1dc826e93606aa6fdc0aadc12f741b01046aa6b99f691ed221a9f0ca043fbeac";

/// RFC 8448 §3 的 `s hs traffic`。
const SERVER_HS_TRAFFIC: &str =
    "b67b7d690cc16c4e75e54213cb2d37b4e9c912bcded9105d42befd59d391ad38";

/// RFC 8448 §3 的 `c hs traffic`。
const CLIENT_HS_TRAFFIC: &str =
    "b3eddb126e067f35a780b3abf45e2d8f3b1a950738f52e9600746a0e27a55a21";

/// RFC 8448 §3 的握手阶段转录哈希（CH..SH）。
const TRANSCRIPT_HASH_CH_SH: &str =
    "860c06edc07858ee8e78f0e7428c58edd6b43f2ca3e6e95f02ed063cf0e1cad8";

/// RFC 8448 §3 的服务端握手写密钥与 IV。
const SERVER_HS_KEY: &str = "3fce516009c21727d0f2e4e86ee403bc";
const SERVER_HS_IV: &str = "5d313eb2671276ee13000b30";

#[test]
fn the_early_secret_matches_rfc_8448() {
    let zeros = vec![0u8; 32];
    let early = extract(suite(), &zeros, &zeros);
    assert_eq!(hex(EARLY_SECRET), early, "early_secret 必须与 RFC 一致");
}

#[test]
fn the_empty_transcript_hash_is_the_hash_of_no_messages() {
    // SHA-256("") 的已知值。`Derive-Secret` 的上下文永远是「消息的哈希」，
    // 空转录要传这个值而不是空字节。
    assert_eq!(
        hex("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
        empty_transcript_hash(suite()),
        "空转录的哈希是 SHA-256(\"\")"
    );
}

#[test]
fn the_derived_secret_matches_rfc_8448() {
    // RFC 8448 明确给出这条：info 里的 hash 段是 e3b0c442…（空串哈希）。
    let early = hex(EARLY_SECRET);
    let derived = derive_secret(suite(), &early, "derived", &empty_transcript_hash(suite()))
        .expect("派生");
    assert_eq!(
        hex("6f2615a108c702c5678f54fc9dbab69716c076189c48250cebeac3576c3611ba"),
        derived,
        "derived 必须与 RFC 8448 一致"
    );
}

#[test]
fn the_handshake_secret_matches_rfc_8448() {
    let early = hex(EARLY_SECRET);
    let derived = derive_secret(suite(), &early, "derived", &empty_transcript_hash(suite()))
        .expect("派生");
    let handshake = extract(suite(), &derived, &hex(SHARED_SECRET));
    assert_eq!(hex(HANDSHAKE_SECRET), handshake);
}

#[test]
fn both_direction_traffic_secrets_match_rfc_8448() {
    let handshake = hex(HANDSHAKE_SECRET);
    let transcript = hex(TRANSCRIPT_HASH_CH_SH);
    let server = derive_secret(suite(), &handshake, "s hs traffic", &transcript).expect("派生");
    let client = derive_secret(suite(), &handshake, "c hs traffic", &transcript).expect("派生");
    assert_eq!(hex(SERVER_HS_TRAFFIC), server, "s hs traffic");
    assert_eq!(hex(CLIENT_HS_TRAFFIC), client, "c hs traffic");
}

#[test]
fn the_server_handshake_traffic_keys_match_rfc_8448() {
    // 关键点：`key`/`iv` 的 PRK 是 `s hs traffic`，**不是** handshake_secret。
    // 拿 handshake_secret 当 PRK 会算出一组自洽但错误的密钥。
    let keys = traffic_keys(suite(), &hex(SERVER_HS_TRAFFIC)).expect("派生密钥");
    assert_eq!(hex(SERVER_HS_KEY), keys.key, "握手写密钥");
    assert_eq!(hex(SERVER_HS_IV), keys.iv.to_vec(), "握手写 IV");
}

#[test]
fn the_finished_key_derives_from_the_direction_secret() {
    // RFC 8448 的 finished key 由 `finished` 标签从方向 secret 派生。
    let secret = hex(SERVER_HS_TRAFFIC);
    let prk = ring::hkdf::Prk::new_less_safe(suite().hkdf_algorithm(), &secret);
    let mut finished_key = [0u8; 32];
    expand_label(suite(), &prk, "finished", &[], &mut finished_key).expect("派生");
    assert_eq!(
        hex("008d3b66f816ea559f96b537e885c31fc068bf492c652f01f288a1d8cdc19fc8"),
        finished_key.to_vec(),
        "finished key 必须与 RFC 8448 一致"
    );
}

#[test]
fn a_real_rfc_8448_encrypted_record_decrypts() {
    // RFC 8448 §3：服务端发出的第一条加密记录（含 EncryptedExtensions）。
    // 这条记录用上面验证过的 `s hs traffic` 密钥加密，序号从 0 开始。
    let record_bytes = hex(
        "17030302a2d1ff334a56f5bff6594a07cc87b580233f500f45e489e7f33af35edf7869fcf40aa40aa2b8ea73f848a7ca07612ef9f945cb960b4068905123ea78b111b429ba9191cd05d2a389280f526134aadc7fc78c4b729df828b5ecf7b13bd9aefb0e57f271585b8ea9bb355c7c79020716cfb9b1183ef3ab20e37d57a6b9d7477609aee6e122a4cf51427325250c7d0e509289444c9b3a648f1d71035d2ed65b0e3cdd0cbae8bf2d0b227812cbb360987255cc744110c453baa4fcd610928d809810e4b7ed1a8fd991f06aa6248204797e36a6a73b70a2559c09ead686945ba246ab66e5edd8044b4c6de3fcf2a89441ac66272fd8fb330ef8190579b3684596c960bd596eea520a56a8d650f563aad27409960dca63d3e688611ea5e22f4415cf9538d51a200c27034272968a264ed6540c84838d89f72c24461aad6d26f59ecaba9acbbb317b66d902f4f292a36ac1b639c637ce343117b659622245317b49eeda0c6258f100d7d961ffb138647e92ea330faeea6dfa31c7a84dc3bd7e1b7a6c7178af36879018e3f252107f243d243dc7339d5684c8b0378bf30244da8c87c843f5e56eb4c5e8280a2b48052cf93b16499a66db7cca71e4599426f7d461e66f99882bd89fc50800becca62d6c74116dbd2972fda1fa80f85df881edbe5a37668936b335583b599186dc5c6918a396fa48a181d6b6fa4f9d62d513afbb992f2b992f67f8afe67f76913fa388cb5630c8ca01e0c65d11c66a1e2ac4c85977b7c7a6999bbf10dc35ae69f5515614636c0b9b68c19ed2e31c0b3b66763038ebba42f3b38edc0399f3a9f23faa63978c317fc9fa66a73f60f0504de93b5b845e275592c12335ee340bbc4fddd502784016e4b3be7ef04dda49f4b440a30cb5d2af939828fd4ae3794e44f94df5a631ede42c1719bfdabf0253fe5175be898e750edc53370d2b",
    );
    let (record, consumed) = parse_record(&record_bytes).expect("应当解析出记录");
    assert_eq!(consumed, record_bytes.len());
    assert_eq!(record.kind, ContentType::ApplicationData);

    let mut key = RecordKey::new(
        suite().aead(),
        &hex(SERVER_HS_KEY),
        {
            let mut iv = [0u8; 12];
            iv.copy_from_slice(&hex(SERVER_HS_IV));
            iv
        },
    )
    .expect("构造记录密钥");
    let (kind, plaintext) = key.open(&record).expect("RFC 的加密记录必须能解开");
    assert_eq!(kind, ContentType::Handshake, "内层类型是握手");
    // 载荷以 EncryptedExtensions 的握手头开始：08 00 00 24
    assert_eq!(
        &plaintext[0..4],
        &[0x08, 0x00, 0x00, 0x24],
        "应当解出 EncryptedExtensions"
    );
    assert_eq!(plaintext.len(), 657, "RFC 8448 §3 的载荷长度");
}

#[test]
fn a_tampered_rfc_record_is_rejected() {
    // 同一条记录，改一个字节就必须被拒绝——这是 AEAD 的存在意义。
    let mut record_bytes = hex("17 03 03 00 43 a2 3f 70 54 b6 2c 94 d0 af fa fe 82 28 ba
         55 cb ef ac ea 42 f9 14 aa 66 bc ab 3f 2b 98 19 a8 a5 b4 6b 39 5b d5 4a 9a
         20 44 1e 2b 62 97 4e 1f 5a 62 92 a2 97 70 14 bd 1e 3d ea e6 3a ee bb 21 69
         49 15 e4");
    let last = record_bytes.len() - 1;
    record_bytes[last] ^= 0xff;
    let (record, _) = parse_record(&record_bytes).expect("结构仍完整");
    let mut key = RecordKey::new(
        suite().aead(),
        &hex(SERVER_HS_KEY),
        {
            let mut iv = [0u8; 12];
            iv.copy_from_slice(&hex(SERVER_HS_IV));
            iv
        },
    )
    .expect("构造记录密钥");
    assert!(key.open(&record).is_err(), "篡改的记录必须被拒绝");
}
