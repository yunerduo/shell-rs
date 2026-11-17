//! 发布清单：安装包命名，以及清单 JSON 的组装。
//!
//! 命令行 `publish` 和后台发布共用这里的包清单与组装逻辑，避免两边对
//! 平台、文件名和字段的写法不一致。签名不在这里，见 `signing.rs`。

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

/// 建议更新：客户端提示，用户可以晚点装。
pub const SEVERITY_RECOMMENDED: &str = "recommended";
/// 强制更新：客户端应尽快装。分级字段由客户端提出，接上之前先原样存进清单。
pub const SEVERITY_REQUIRED: &str = "required";

/// 一个平台的包：升级用的资产，和给人手动装的安装包可以是同一个文件。
pub struct Package {
    pub platform: &'static str,
    pub file: String,
    pub asset: bool,
    pub installer: bool,
}

/// 一个已存在的包的体积与摘要。
#[derive(Clone, Debug)]
pub struct FileInfo {
    pub size: u64,
    pub sha256: String,
}

/// 一个版本可能包含的包。缺哪个就跳过哪个，清单里只列存在的。
pub fn packages(version: &str) -> Vec<Package> {
    let setup = format!("ShellAs-{version}-windows-x86_64-setup.exe");
    let portable = format!("ShellAs-{version}-windows-x86_64-portable.zip");
    let mac_arm = format!("ShellAs-{version}-macos-aarch64.app.zip");
    let mac_intel = format!("ShellAs-{version}-macos-x86_64.app.zip");
    let dmg_arm = format!("ShellAs-{version}-macos-aarch64.dmg");
    let dmg_intel = format!("ShellAs-{version}-macos-x86_64.dmg");
    let linux = format!("ShellAs-{version}-linux-x86_64.AppImage");
    vec![
        Package {
            platform: "windows-x86_64",
            file: setup,
            asset: true,
            installer: true,
        },
        Package {
            platform: "windows-x86_64-portable",
            file: portable,
            asset: true,
            installer: true,
        },
        Package {
            platform: "macos-aarch64",
            file: mac_arm,
            asset: true,
            installer: false,
        },
        Package {
            platform: "macos-aarch64",
            file: dmg_arm,
            asset: false,
            installer: true,
        },
        Package {
            platform: "macos-x86_64",
            file: mac_intel,
            asset: true,
            installer: false,
        },
        Package {
            platform: "macos-x86_64",
            file: dmg_intel,
            asset: false,
            installer: true,
        },
        Package {
            platform: "linux-x86_64",
            file: linux,
            asset: true,
            installer: true,
        },
    ]
}

pub fn valid_channel(channel: &str) -> bool {
    channel == "stable" || channel == "beta"
}

/// 清单里按平台分的资产名，拆成给人看的平台与架构。
pub fn split_platform(platform: &str) -> (&'static str, &'static str) {
    if platform.starts_with("windows") {
        ("windows", "x86_64")
    } else if platform == "macos-aarch64" {
        ("macos", "aarch64")
    } else if platform.starts_with("macos") {
        ("macos", "x86_64")
    } else if platform.starts_with("linux") {
        ("linux", "x86_64")
    } else {
        ("", "")
    }
}

/// 这个版本里某个文件名属于哪个平台；不认识的返回 `None`。
pub fn file_platform(version: &str, file: &str) -> Option<&'static str> {
    packages(version)
        .into_iter()
        .find(|package| package.file == file)
        .map(|package| package.platform)
}

/// 分级只认客户端约定的两个值，不接 mandatory / force 之类的别名。
pub fn valid_severity(severity: &str) -> bool {
    matches!(severity, SEVERITY_RECOMMENDED | SEVERITY_REQUIRED)
}

/// 组装清单 JSON。`found` 以包的文件名为键，只有实际存在的包才进清单。
#[allow(clippy::too_many_arguments)]
pub fn build_manifest(
    base: &str,
    channel: &str,
    version: &str,
    severity: &str,
    notes: &str,
    notes_url: Option<&str>,
    minimum_version: Option<&str>,
    rollout: f64,
    published_at: &str,
    found: &BTreeMap<String, FileInfo>,
) -> Value {
    let mut assets = Map::new();
    let mut installers = Map::new();
    for package in packages(version) {
        let Some(file) = found.get(&package.file) else {
            continue;
        };
        let url = format!("{base}/releases/{version}/{}", package.file);
        if package.asset {
            assets.insert(
                package.platform.to_string(),
                json!({
                    "urls": [url.clone()],
                    "size": file.size,
                    "sha256": file.sha256,
                }),
            );
        }
        if package.installer {
            installers.insert(package.platform.to_string(), json!(url));
        }
    }
    json!({
        "schema": 1,
        "channel": channel,
        "version": version,
        "published_at": published_at,
        "severity": severity,
        "minimum_version": minimum_version,
        "rollout": rollout,
        "notes": notes,
        "notes_url": notes_url,
        "assets": assets,
        "installers": installers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(size: u64) -> FileInfo {
        FileInfo {
            size,
            sha256: "a".repeat(64),
        }
    }

    #[test]
    fn platform_names_split_into_platform_and_arch() {
        assert_eq!(split_platform("windows-x86_64"), ("windows", "x86_64"));
        assert_eq!(
            split_platform("windows-x86_64-portable"),
            ("windows", "x86_64")
        );
        assert_eq!(split_platform("macos-aarch64"), ("macos", "aarch64"));
        assert_eq!(split_platform("macos-x86_64"), ("macos", "x86_64"));
        assert_eq!(split_platform("linux-x86_64"), ("linux", "x86_64"));
        assert_eq!(split_platform("unknown"), ("", ""));
    }

    #[test]
    fn the_package_names_match_the_build() {
        let names: Vec<String> = packages("0.1.19")
            .into_iter()
            .map(|package| package.file)
            .collect();
        assert!(names.contains(&"ShellAs-0.1.19-windows-x86_64-setup.exe".to_string()));
        assert!(names.contains(&"ShellAs-0.1.19-macos-aarch64.dmg".to_string()));
        assert!(names.contains(&"ShellAs-0.1.19-linux-x86_64.AppImage".to_string()));
    }

    #[test]
    fn only_files_that_exist_enter_the_manifest() {
        let mut found = BTreeMap::new();
        found.insert(
            "ShellAs-0.1.20-windows-x86_64-setup.exe".to_string(),
            file(1024),
        );
        let manifest = build_manifest(
            "https://example.test",
            "stable",
            "0.1.20",
            SEVERITY_REQUIRED,
            "说明",
            Some("https://example.test/changelog#v0.1.20"),
            None,
            1.0,
            "2026-11-16T00:00:00Z",
            &found,
        );
        assert_eq!(manifest["severity"], "required");
        assert!(manifest["assets"].get("windows-x86_64").is_some());
        assert!(manifest["assets"].get("linux-x86_64").is_none());
        assert_eq!(
            manifest["assets"]["windows-x86_64"]["size"].as_u64(),
            Some(1024)
        );
    }

    #[test]
    fn severity_only_accepts_the_agreed_values() {
        assert!(valid_severity(SEVERITY_RECOMMENDED));
        assert!(valid_severity(SEVERITY_REQUIRED));
        assert!(!valid_severity("suggested"));
        assert!(!valid_severity("mandatory"));
        assert!(!valid_severity("force"));
        assert!(!valid_severity(""));
    }
}
