//! 读取签名安装包上的"标签"(与服务端 internal/petag 同一格式)。
//!
//! 安装包在下发时按代理站点打上 `portal=https://<域名>`,写在 Authenticode
//! 证书表的末尾 —— 那一段不在签名哈希的覆盖范围内,所以同一个签名文件可以
//! 每个站点各打一份而签名仍然有效(Chrome 各渠道就是这么分发的)。
//! 安装完成时 NSIS 把安装包路径交给启动器(`--import-installer-tag <path>`),
//! 这里读出 portal 写进 api-base,这台机器从此知道自己归哪个站点。
//!
//! 标签格式:证书表尾部 `"RCXTAG1\0"` + u16 LE 长度 + UTF-8 载荷。
//! 只读不写;找不到标签是正常情况(主站直接下载的包、老版本安装包)。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const MAGIC: &[u8] = b"RCXTAG1\0";
const MAX_PAYLOAD: usize = 1024;
const PE_POINTER_OFFSET: u64 = 0x3c;
const COFF_HEADER_SIZE: u64 = 20;
const OPTIONAL_MAGIC_PE32: u16 = 0x10b;
const OPTIONAL_MAGIC_PE32_PLUS: u16 = 0x20b;
const DATA_DIR_OFFSET_PE32: u64 = 96;
const DATA_DIR_OFFSET_PE32_PLUS: u64 = 112;
const SECURITY_DIR_INDEX: u64 = 4;
/// 证书表读取上限:真实签名表几 KB,超过这个数就不是我们认识的文件。
const MAX_CERT_TABLE: u64 = 1 << 20;

fn read_at<R: Read + Seek>(r: &mut R, off: u64, buf: &mut [u8]) -> std::io::Result<()> {
    r.seek(SeekFrom::Start(off))?;
    r.read_exact(buf)
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// 从任意 PE 读标签载荷。不是 PE、未签名、没有标签一律 `None`。
pub fn read_tag<R: Read + Seek>(r: &mut R) -> Option<Vec<u8>> {
    let mut b4 = [0u8; 4];
    read_at(r, PE_POINTER_OFFSET, &mut b4).ok()?;
    let pe_off = u32::from_le_bytes(b4) as u64;
    read_at(r, pe_off, &mut b4).ok()?;
    if &b4 != b"PE\0\0" {
        return None;
    }
    let mut coff = [0u8; 20];
    read_at(r, pe_off + 4, &mut coff).ok()?;
    let opt_size = u16_at(&coff, 16) as u64;
    let opt_off = pe_off + 4 + COFF_HEADER_SIZE;
    let mut b2 = [0u8; 2];
    read_at(r, opt_off, &mut b2).ok()?;
    let dir_off = match u16::from_le_bytes(b2) {
        OPTIONAL_MAGIC_PE32 => DATA_DIR_OFFSET_PE32,
        OPTIONAL_MAGIC_PE32_PLUS => DATA_DIR_OFFSET_PE32_PLUS,
        _ => return None,
    };
    let entry_off = opt_off + dir_off + SECURITY_DIR_INDEX * 8;
    if entry_off + 8 > opt_off + opt_size {
        return None;
    }
    let mut entry = [0u8; 8];
    read_at(r, entry_off, &mut entry).ok()?;
    let cert_off = u32_at(&entry, 0) as u64;
    let cert_size = u32_at(&entry, 4) as u64;
    if cert_off == 0 || cert_size == 0 || cert_size > MAX_CERT_TABLE {
        return None;
    }
    let mut table = vec![0u8; cert_size as usize];
    read_at(r, cert_off, &mut table).ok()?;
    let idx = table
        .windows(MAGIC.len())
        .rposition(|w| w == MAGIC)?;
    let rest = &table[idx + MAGIC.len()..];
    if rest.len() < 2 {
        return None;
    }
    let n = u16_at(rest, 0) as usize;
    if n == 0 || n > MAX_PAYLOAD || rest.len() < 2 + n {
        return None;
    }
    Some(rest[2..2 + n].to_vec())
}

fn http_origin(value: &str) -> Option<String> {
    let origin = value.trim();
    if origin.starts_with("https://") || origin.starts_with("http://") {
        Some(origin.trim_end_matches('/').to_owned())
    } else {
        None
    }
}

/// 读安装包文件里内嵌的 portal(`portal=https://...`);其他形状的载荷不认。
pub fn read_embedded_portal(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let payload = read_tag(&mut file)?;
    let text = std::str::from_utf8(&payload).ok()?;
    http_origin(text.strip_prefix("portal=")?)
}

/// 文件名线索:下发时把站点写在文件名里,`ReCodex-1.2.64-windows-x64-setup@youde.pro.exe`。
/// `@` 后到扩展名之前就是域名。浏览器保留服务端给的文件名;用户改名就没了 —— 所以只是第二道。
pub fn portal_from_filename(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    let (_, host) = stem.rsplit_once('@')?;
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return None;
    }
    Some(format!("https://{host}"))
}

/// 下载来源线索(Windows):浏览器给下载的文件写 `Zone.Identifier` 备用数据流,
/// Chrome/Edge 会记 `HostUrl=<下载地址>`。下载地址在代理域名上,origin 就是站点。
/// 第三道:第三方下载器不写、企业策略可能剥掉。
pub fn portal_from_zone_identifier(path: &Path) -> Option<String> {
    let mut ads = path.as_os_str().to_owned();
    ads.push(":Zone.Identifier");
    let text = std::fs::read_to_string(std::path::PathBuf::from(ads)).ok()?;
    let url = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("HostUrl="))?
        .trim();
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next()?;
    if host.is_empty() {
        return None;
    }
    let scheme = if url.starts_with("https://") { "https" } else { "http" };
    Some(format!("{scheme}://{host}"))
}

/// 三道线索按可信度取第一个命中:内嵌标签 > 文件名 > 下载来源。
/// 全部落空返回 None —— 那就交给登录时的最后防线(显示验证码,授权后回写归属)。
pub fn read_portal(path: &Path) -> Option<String> {
    read_embedded_portal(path)
        .or_else(|| portal_from_filename(path))
        .or_else(|| portal_from_zone_identifier(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// 与 Go 侧 petag_test 的 buildSignedPE 同构:最小 PE32+ 骨架 + 一个 WIN_CERTIFICATE。
    fn build_pe(cert_body: &[u8], tag: Option<&[u8]>) -> Vec<u8> {
        let pe_off = 0x80usize;
        let opt_size = DATA_DIR_OFFSET_PE32_PLUS as usize + 16 * 8;
        let header_end = pe_off + 4 + COFF_HEADER_SIZE as usize + opt_size;
        let mut code = vec![0x90u8; 64];
        let mut cert_start = header_end + code.len();
        while cert_start % 8 != 0 {
            code.push(0);
            cert_start += 1;
        }
        let mut body = cert_body.to_vec();
        if let Some(tag) = tag {
            body.extend_from_slice(MAGIC);
            body.extend_from_slice(&(tag.len() as u16).to_le_bytes());
            body.extend_from_slice(tag);
        }
        let cert_len = 8 + body.len();
        let padded = (cert_len + 7) & !7;

        let mut f = vec![0u8; header_end];
        f[..2].copy_from_slice(b"MZ");
        f[PE_POINTER_OFFSET as usize..PE_POINTER_OFFSET as usize + 4]
            .copy_from_slice(&(pe_off as u32).to_le_bytes());
        f[pe_off..pe_off + 4].copy_from_slice(b"PE\0\0");
        let coff = pe_off + 4;
        f[coff + 16..coff + 18].copy_from_slice(&(opt_size as u16).to_le_bytes());
        let opt = coff + COFF_HEADER_SIZE as usize;
        f[opt..opt + 2].copy_from_slice(&OPTIONAL_MAGIC_PE32_PLUS.to_le_bytes());
        let entry = opt + DATA_DIR_OFFSET_PE32_PLUS as usize + SECURITY_DIR_INDEX as usize * 8;
        f[entry..entry + 4].copy_from_slice(&(cert_start as u32).to_le_bytes());
        f[entry + 4..entry + 8].copy_from_slice(&(padded as u32).to_le_bytes());
        f.extend_from_slice(&code);
        let mut cert = vec![0u8; padded];
        cert[..4].copy_from_slice(&(cert_len as u32).to_le_bytes());
        cert[4..6].copy_from_slice(&0x0200u16.to_le_bytes());
        cert[6..8].copy_from_slice(&0x0002u16.to_le_bytes());
        cert[8..8 + body.len()].copy_from_slice(&body);
        f.extend_from_slice(&cert);
        f
    }

    #[test]
    fn reads_portal_from_tagged_pe() {
        let pe = build_pe(&[0xAB; 37], Some(b"portal=https://youde.pro"));
        assert_eq!(
            read_tag(&mut Cursor::new(&pe)).as_deref(),
            Some(&b"portal=https://youde.pro"[..])
        );
    }

    #[test]
    fn untagged_and_garbage_yield_none() {
        let pe = build_pe(&[0xAB; 37], None);
        assert!(read_tag(&mut Cursor::new(&pe)).is_none());
        assert!(read_tag(&mut Cursor::new(b"not a pe at all".to_vec())).is_none());
    }

    #[test]
    fn read_portal_requires_http_origin() {
        let dir = std::env::temp_dir().join(format!("rcx-tag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.exe");
        std::fs::write(&good, build_pe(&[1; 9], Some(b"portal=https://youde.pro"))).unwrap();
        assert_eq!(read_portal(&good).as_deref(), Some("https://youde.pro"));
        let bad = dir.join("bad.exe");
        std::fs::write(&bad, build_pe(&[1; 9], Some(b"portal=javascript:alert(1)"))).unwrap();
        assert!(read_portal(&bad).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filename_hint_parses_host_after_at_sign() {
        let p = Path::new("C:/Users/x/Downloads/ReCodex-1.2.64-windows-x64-setup@youde.pro.exe");
        assert_eq!(portal_from_filename(p).as_deref(), Some("https://youde.pro"));
        assert_eq!(
            portal_from_filename(Path::new("ReCodex-1.2.64-macos-arm64@Recodex.JzSpace.cn.dmg")).as_deref(),
            Some("https://recodex.jzspace.cn")
        );
        // 没有 @、或 @ 后不是域名字符,一律不认
        assert!(portal_from_filename(Path::new("ReCodex-1.2.64-windows-x64-setup.exe")).is_none());
        assert!(portal_from_filename(Path::new("setup@evil host.exe")).is_none());
        assert!(portal_from_filename(Path::new("setup@.exe")).is_none());
    }

    #[test]
    fn zone_identifier_hint_takes_host_from_hosturl() {
        let dir = std::env::temp_dir().join(format!("rcx-zone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("setup.exe");
        std::fs::write(&exe, b"x").unwrap();
        let mut ads = exe.as_os_str().to_owned();
        ads.push(":Zone.Identifier");
        // NTFS 备用数据流;非 NTFS 卷写不进去就跳过这条用例
        if std::fs::write(
            std::path::PathBuf::from(&ads),
            "[ZoneTransfer]\r\nZoneId=3\r\nReferrerUrl=https://youde.pro/download\r\nHostUrl=https://youde.pro/api/v1/client/download/windows\r\n",
        )
        .is_ok()
        {
            assert_eq!(portal_from_zone_identifier(&exe).as_deref(), Some("https://youde.pro"));
            assert_eq!(read_portal(&exe).as_deref(), Some("https://youde.pro"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
