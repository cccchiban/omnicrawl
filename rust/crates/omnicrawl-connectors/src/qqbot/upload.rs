//! 富媒体分片上传的本地准备：MD5 校验值、分片切分与预上传请求体。
//!
//! 平台二进制直传（`file_data`）上限 5MB，更大的文件要先 `upload_prepare` 拿分片预签名
//! 地址，逐片 PUT，再逐片通知完成，最后用 `upload_id` 调一次上传接口合并。预上传要求整
//! 文件 MD5、整文件 SHA-1 与前 10MB 的 MD5，因此这里自带一份 MD5（仓库里已有手写的
//! SHA-1，见 [`crate::ws::sha1`]，同样不引依赖）。

use serde_json::{json, Value};

/// 预上传取样长度：平台要求「文件前 10002432 字节（约 10MB）的 MD5」。
pub const MD5_10M_BYTES: usize = 10_002_432;

/// 分片预上传要提交的整文件校验值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksums {
    pub md5: String,
    pub sha1: String,
    pub md5_10m: String,
}

/// 算一份文件的校验值；不足 [`MD5_10M_BYTES`] 时前段取样就是整个文件。
pub fn checksums(bytes: &[u8]) -> Checksums {
    let head = &bytes[..bytes.len().min(MD5_10M_BYTES)];
    Checksums {
        md5: hex(&md5(bytes)),
        sha1: hex(&crate::ws::sha1(bytes)),
        md5_10m: hex(&md5(head)),
    }
}

/// 小写十六进制。
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 预上传请求体：`file_size` 与 `md5_10m` 按平台口径发字符串。
pub fn prepare_body(file_type: i64, file_name: &str, bytes: &[u8]) -> Value {
    let sums = checksums(bytes);
    json!({
        "file_type": file_type,
        "file_size": bytes.len().to_string(),
        "file_name": file_name,
        "md5": sums.md5,
        "sha1": sums.sha1,
        "md5_10m": sums.md5_10m,
    })
}

/// 按 `block_size` 切出 `(偏移, 长度)`；空文件没有分片。
pub fn plan_parts(size: usize, block_size: usize) -> Vec<(usize, usize)> {
    if block_size == 0 || size == 0 {
        return Vec::new();
    }
    (0..size)
        .step_by(block_size)
        .map(|offset| (offset, (size - offset).min(block_size)))
        .collect()
}

/// MD5（RFC 1321）。
pub fn md5(data: &[u8]) -> [u8; 16] {
    const K: [u32; 64] = [
        0xd76a_a478,
        0xe8c7_b756,
        0x2420_70db,
        0xc1bd_ceee,
        0xf57c_0faf,
        0x4787_c62a,
        0xa830_4613,
        0xfd46_9501,
        0x6980_98d8,
        0x8b44_f7af,
        0xffff_5bb1,
        0x895c_d7be,
        0x6b90_1122,
        0xfd98_7193,
        0xa679_438e,
        0x49b4_0821,
        0xf61e_2562,
        0xc040_b340,
        0x265e_5a51,
        0xe9b6_c7aa,
        0xd62f_105d,
        0x0244_1453,
        0xd8a1_e681,
        0xe7d3_fbc8,
        0x21e1_cde6,
        0xc337_07d6,
        0xf4d5_0d87,
        0x455a_14ed,
        0xa9e3_e905,
        0xfcef_a3f8,
        0x676f_02d9,
        0x8d2a_4c8a,
        0xfffa_3942,
        0x8771_f681,
        0x6d9d_6122,
        0xfde5_380c,
        0xa4be_ea44,
        0x4bde_cfa9,
        0xf6bb_4b60,
        0xbebf_bc70,
        0x289b_7ec6,
        0xeaa1_27fa,
        0xd4ef_3085,
        0x0488_1d05,
        0xd9d4_d039,
        0xe6db_99e5,
        0x1fa2_7cf8,
        0xc4ac_5665,
        0xf429_2244,
        0x432a_ff97,
        0xab94_23a7,
        0xfc93_a039,
        0x655b_59c3,
        0x8f0c_cc92,
        0xffef_f47d,
        0x8584_5dd1,
        0x6fa8_7e4f,
        0xfe2c_e6e0,
        0xa301_4314,
        0x4e08_11a1,
        0xf753_7e82,
        0xbd3a_f235,
        0x2ad7_d2bb,
        0xeb86_d391,
    ];
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];

    let mut message = data.to_vec();
    let bit_length = (data.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_le_bytes());

    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    for chunk in message.chunks(64) {
        let mut words = [0_u32; 16];
        for (index, slot) in words.iter_mut().enumerate() {
            *slot = u32::from_le_bytes([
                chunk[index * 4],
                chunk[index * 4 + 1],
                chunk[index * 4 + 2],
                chunk[index * 4 + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (state[0], state[1], state[2], state[3]);
        for index in 0..64 {
            let (f, g) = match index / 16 {
                0 => ((b & c) | (!b & d), index),
                1 => ((d & b) | (!d & c), (5 * index + 1) % 16),
                2 => (b ^ c ^ d, (3 * index + 5) % 16),
                _ => (c ^ (b | !d), (7 * index) % 16),
            };
            let rotated = a
                .wrapping_add(f)
                .wrapping_add(K[index])
                .wrapping_add(words[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(rotated.rotate_left(S[index]));
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }

    let mut digest = [0_u8; 16];
    for (index, value) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_known_vectors() {
        assert_eq!(hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(&md5(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            )),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    #[test]
    fn md5_handles_a_full_block_boundary() {
        // 55/56/64 字节是填充分支的分界，容易写错。
        assert_eq!(
            hex(&md5(&[b'a'; 55])),
            "ef1772b6dff9a122358552954ad0df65".to_string()
        );
        assert_eq!(
            hex(&md5(&[b'a'; 56])),
            "3b0c8ac703f828b04c6c197006d17218".to_string()
        );
        assert_eq!(
            hex(&md5(&[b'a'; 64])),
            "014842d480b571495a4a0363793f7367".to_string()
        );
    }

    #[test]
    fn sha1_comes_from_the_existing_helper() {
        let sums = checksums(b"abc");
        assert_eq!(sums.sha1, "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(sums.md5, "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn the_ten_megabyte_window_covers_short_files_whole() {
        let small = vec![7_u8; 1024];
        let sums = checksums(&small);
        assert_eq!(sums.md5_10m, sums.md5);
    }

    #[test]
    fn the_ten_megabyte_window_stops_at_the_platform_boundary() {
        let mut big = vec![0_u8; MD5_10M_BYTES + 1024];
        big[MD5_10M_BYTES] = 1;
        let extended = checksums(&big);
        // 尾部的额外字节只改变整文件哈希，不影响前 10MB 取样。
        let mut truncated = big.clone();
        truncated.truncate(MD5_10M_BYTES);
        let exact = checksums(&truncated);
        assert_eq!(extended.md5_10m, exact.md5);
        assert_ne!(extended.md5, exact.md5);
    }

    #[test]
    fn prepare_body_uses_string_sizes() {
        let body = prepare_body(2, "demo.mp4", b"abc");
        assert_eq!(body["file_type"], json!(2));
        assert_eq!(body["file_size"], json!("3"));
        assert_eq!(body["file_name"], json!("demo.mp4"));
        assert_eq!(body["md5"], json!("900150983cd24fb0d6963f7d28e17f72"));
        assert_eq!(
            body["sha1"],
            json!("a9993e364706816aba3e25717850c26c9cd0d89d")
        );
    }

    #[test]
    fn parts_tile_the_file_exactly() {
        assert_eq!(plan_parts(0, 5), Vec::new());
        assert_eq!(plan_parts(10, 5), vec![(0, 5), (5, 5)]);
        // 末片不足一整块。
        assert_eq!(plan_parts(12, 5), vec![(0, 5), (5, 5), (10, 2)]);
        assert_eq!(plan_parts(3, 5), vec![(0, 3)]);
        assert_eq!(plan_parts(5, 0), Vec::new());
    }

    #[test]
    fn parts_cover_every_byte_once() {
        let size = 1024 * 1024 + 17;
        let parts = plan_parts(size, 400 * 1024);
        assert_eq!(parts.iter().map(|(_, len)| len).sum::<usize>(), size);
        let mut next = 0;
        for (offset, len) in parts {
            assert_eq!(offset, next);
            next += len;
        }
    }
}
