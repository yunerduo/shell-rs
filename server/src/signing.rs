//! 清单签名。私钥只放在发布机器或服务器本地，服务器的管理接口从不接收
//! 私钥；trusted comment 与客户端 `src/update/verify.rs` 的约定一致。

use std::path::Path;

use anyhow::{Context, Result};

/// 给清单原文签一个 minisign 签名。
pub fn sign(
    key: &Path,
    password: Option<&str>,
    channel: &str,
    version: &str,
    manifest: &str,
) -> Result<String> {
    if !key.is_file() {
        anyhow::bail!("找不到 minisign 私钥 {}", key.display());
    }
    // minisign 在 password 为 None 时会从终端交互式读口令；服务器与后台
    // 没有终端，传空字符串表示「私钥不带口令」，带口令的私钥必须显式配置。
    let secret =
        minisign::SecretKey::from_file(key, Some(password.unwrap_or_default().to_string()))
            .context("读取 minisign 私钥失败（口令可能不对）")?;
    let comment = format!("shellas-manifest {channel} {version}");
    let signature = minisign::sign(
        None,
        &secret,
        manifest.as_bytes(),
        Some(&comment),
        Some("signature from Agree ShellAs update key"),
    )?;
    Ok(signature.into_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_key_signs_a_manifest_that_verifies() {
        // 空口令生成的是不带口令的私钥，但 checksum 是有效的。
        let pair = minisign::KeyPair::generate_encrypted_keypair(Some(String::new())).unwrap();
        let dir = std::env::temp_dir().join(format!("shellas-sign-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("test.key");
        std::fs::write(
            &key,
            pair.sk
                .to_box(Some("shellas test key"))
                .unwrap()
                .into_string(),
        )
        .unwrap();

        let manifest =
            r#"{"schema":1,"channel":"stable","version":"0.1.20","severity":"required"}"#;
        let signature = sign(&key, None, "stable", "0.1.20", manifest).unwrap();
        let signature_box = minisign::SignatureBox::from_string(&signature).unwrap();
        minisign::verify(
            &pair.pk,
            &signature_box,
            std::io::Cursor::new(manifest.as_bytes()),
            true,
            false,
            false,
        )
        .unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_key_is_reported() {
        let error = sign(
            Path::new("/nonexistent/shellas-update.key"),
            None,
            "stable",
            "0.1.20",
            "{}",
        )
        .unwrap_err();
        assert!(error.to_string().contains("找不到"));
    }
}
