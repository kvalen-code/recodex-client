//! 跟随账号配对的确认码(docs/remote-app-plan.md §2.3,三端必须一致):
//!
//! ```text
//! code = (SHA-256(公钥原始 32 字节) 的前 4 字节按大端读成 uint32) mod 1_000_000
//! ```
//! 显示为 6 位、左补 0,中间空一格:「482 193」。
//!
//! 这里**刻意**在本机从公钥自己算,而不是直接显示后台回的 code:确认码存在的意义就是
//! 防后台掉包公钥 —— 显示后台给的码等于不设防。两边对不上必须当场中止配对。

use base64::Engine as _;
use sha2::{Digest, Sha256};

/// 纯 6 位数字(`"003172"`)。
pub fn confirm_code(public_key: &[u8]) -> String {
    let digest = Sha256::digest(public_key);
    let head = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    format!("{:06}", head % 1_000_000)
}

/// 「482 193」;其它形状原样返回。
pub fn format_confirm_code(code: &str) -> String {
    if code.len() == 6 && code.is_ascii() {
        format!("{} {}", &code[..3], &code[3..])
    } else {
        code.to_string()
    }
}

/// 与后台、命令行同口径:只认**带填充的标准 base64**,且必须恰好 32 字节。
/// 放宽的话同一把钥匙有多种写法,三端各算各的就会出岔子。
pub fn decode_public_key(raw: &str) -> Option<[u8; 32]> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(raw).ok()?;
    bytes.try_into().ok()
}

pub fn encode_public_key(key: &[u8; 32]) -> String {
    base64::engine::general_purpose::STANDARD.encode(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 方案 §2.3 的三条固定向量(后台 TestRemotePairCodeFixedVector 同一组)。
    /// 第二条故意选了前 4 字节 ≥ 2³¹ 且需要补 0 的。
    #[test]
    fn fixed_vectors_from_the_plan() {
        let ascending: Vec<u8> = (0u8..32).collect();
        assert_eq!(confirm_code(&ascending), "848873");
        assert_eq!(confirm_code(&[0x15; 32]), "003172");
        assert_eq!(confirm_code(&[0x00; 32]), "123181");
    }

    #[test]
    fn fixed_vectors_through_the_base64_form() {
        for (b64, code) in [
            ("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=", "848873"),
            ("FRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRU=", "003172"),
            ("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", "123181"),
        ] {
            let key = decode_public_key(b64).expect(b64);
            assert_eq!(confirm_code(&key), code);
            assert_eq!(encode_public_key(&key), b64, "重新编码必须回到同一写法");
        }
    }

    #[test]
    fn display_form_has_a_space_in_the_middle() {
        assert_eq!(format_confirm_code("003172"), "003 172");
        assert_eq!(format_confirm_code("12345"), "12345");
    }

    #[test]
    fn only_padded_standard_base64_of_32_bytes_is_accepted() {
        // 无填充
        assert!(decode_public_key("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8").is_none());
        // base64url 字符
        assert!(decode_public_key("FRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFR_=").is_none());
        // 31 字节
        assert!(decode_public_key("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==").is_none());
        // 33 字节
        assert!(decode_public_key("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").is_none());
        assert!(decode_public_key("").is_none());
    }
}
