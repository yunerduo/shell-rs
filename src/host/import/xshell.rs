//! 导入 Xshell 会话。
//!
//! Xshell 把一台主机存成 `Sessions` 目录里的一棵树：一个目录是一个分组，
//! 一个 `.xsh` 文件是一台主机。`.xts` 是它导出的容器，实际是个 zip，里面的
//! 条目仍是 `.xsh`，目录层级就是分组。`.xsh` 是 INI，Windows 上通常是
//! UTF-16LE 带 BOM；`.xts` 的 zip 条目名不带 UTF-8 标记时按 GBK 编码。

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use encoding_rs::GBK;
use zip::ZipArchive;

use super::{
    DiscardedField, ImportCandidate, ImportLoad, ImportProtocol, ImportScan, SkipReason,
    SkippedSession,
};

/// 一个 `.xsh` 只有几十 KB；超过这个大小的条目不再读取。
const MAX_SESSION_BYTES: u64 = 4 * 1024 * 1024;

/// Xshell 默认的 `Sessions` 目录。找不到时给出版本 7 的位置，让界面显示
/// 一个不存在的路径，而不是猜一个用户目录。
pub(super) fn default_sessions_dir() -> PathBuf {
    let documents = dirs::document_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_default();
    let candidates = [
        documents.join("NetSarang Computer/8/Xshell/Sessions"),
        documents.join("NetSarang Computer/7/Xshell/Sessions"),
        documents.join("NetSarang Computer/6/Xshell/Sessions"),
        documents.join("NetSarang/Xshell/Sessions"),
    ];
    candidates
        .iter()
        .find(|path| path.is_dir())
        .cloned()
        .unwrap_or_else(|| candidates[1].clone())
}

/// 读取用户选中的 `.xts`、`.xsh` 或 `Sessions` 文件夹。
pub(super) fn scan(path: &Path) -> ImportLoad {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ImportLoad::Missing;
        }
        Err(error) => return ImportLoad::Failed(error.to_string()),
    };
    if metadata.is_dir() {
        return scan_dir(path);
    }
    match extension(path).as_deref() {
        Some("xsh") => scan_single(path),
        Some("xts") => scan_archive(path),
        _ => ImportLoad::Failed("not an Xshell session file (.xsh or .xts)".to_string()),
    }
}

fn scan_dir(root: &Path) -> ImportLoad {
    let base = xshell_base(root);
    let mut files = Vec::new();
    let mut scan = ImportScan::default();
    collect(root, root, &mut files, &mut scan.skipped);
    files.sort();
    for (path, group) in files {
        read_file(&path, group, base.as_deref(), &mut scan);
    }
    ImportLoad::Loaded(scan)
}

fn collect(
    root: &Path,
    dir: &Path,
    files: &mut Vec<(PathBuf, String)>,
    skipped: &mut Vec<SkippedSession>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            skipped.push(SkippedSession {
                name: display_name(dir),
                reason: SkipReason::Unreadable(error.to_string()),
            });
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                skipped.push(SkippedSession {
                    name: display_name(dir),
                    reason: SkipReason::Unreadable(error.to_string()),
                });
                continue;
            }
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                skipped.push(SkippedSession {
                    name: display_name(&path),
                    reason: SkipReason::Unreadable(error.to_string()),
                });
                continue;
            }
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect(root, &path, files, skipped);
        } else if extension(&path).as_deref() == Some("xsh") {
            files.push((path.clone(), relative_group(root, &path)));
        }
    }
}

fn scan_single(path: &Path) -> ImportLoad {
    let group = single_group(path);
    let base = xshell_base(path);
    let mut scan = ImportScan::default();
    read_file(path, group, base.as_deref(), &mut scan);
    ImportLoad::Loaded(scan)
}

fn read_file(path: &Path, group: String, base: Option<&Path>, scan: &mut ImportScan) {
    let name = display_name(path);
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            scan.skipped.push(SkippedSession {
                name,
                reason: SkipReason::Unreadable(error.to_string()),
            });
            return;
        }
    };
    if metadata.len() > MAX_SESSION_BYTES {
        scan.skipped.push(SkippedSession {
            name,
            reason: SkipReason::Unreadable("session file is too large".to_string()),
        });
        return;
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            scan.skipped.push(SkippedSession {
                name,
                reason: SkipReason::Unreadable(error.to_string()),
            });
            return;
        }
    };
    push_session(&bytes, name, group, base, scan);
}

fn scan_archive(path: &Path) -> ImportLoad {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) => return ImportLoad::Failed(error.to_string()),
    };
    let mut archive = match ZipArchive::new(file) {
        Ok(archive) => archive,
        Err(error) => return ImportLoad::Failed(error.to_string()),
    };
    let base = xshell_base(path);
    let mut scan = ImportScan::default();
    for index in 0..archive.len() {
        let mut entry = match archive.by_index(index) {
            Ok(entry) => entry,
            Err(error) => {
                scan.skipped.push(SkippedSession {
                    name: display_name(path),
                    reason: SkipReason::Unreadable(error.to_string()),
                });
                continue;
            }
        };
        if entry.is_dir() {
            continue;
        }
        let entry_name = decode_entry_name(entry.name_raw());
        let Some((group, file_name)) = archive_entry(&entry_name) else {
            continue;
        };
        if entry.size() > MAX_SESSION_BYTES {
            scan.skipped.push(SkippedSession {
                name: file_name,
                reason: SkipReason::Unreadable("session file is too large".to_string()),
            });
            continue;
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        if let Err(error) = entry
            .by_ref()
            .take(MAX_SESSION_BYTES)
            .read_to_end(&mut bytes)
        {
            scan.skipped.push(SkippedSession {
                name: file_name,
                reason: SkipReason::Unreadable(error.to_string()),
            });
            continue;
        }
        let name = Path::new(&file_name)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or(file_name);
        push_session(&bytes, name, group, base.as_deref(), &mut scan);
    }
    ImportLoad::Loaded(scan)
}

fn push_session(
    bytes: &[u8],
    name: String,
    group: String,
    base: Option<&Path>,
    scan: &mut ImportScan,
) {
    match parse_session(bytes, name.clone(), group, base) {
        Ok(candidate) => scan.candidates.push(candidate),
        Err(reason) => scan.skipped.push(SkippedSession { name, reason }),
    }
}

/// 解析一个 `.xsh`。失败只影响这一台主机，调用方继续读下一个文件。
fn parse_session(
    bytes: &[u8],
    name: String,
    group: String,
    base: Option<&Path>,
) -> Result<ImportCandidate, SkipReason> {
    let text = decode_text(bytes)
        .ok_or_else(|| SkipReason::Unreadable("unsupported text encoding".to_string()))?;
    let ini = Ini::parse(&text);
    if !ini.has_section("CONNECTION") {
        return Err(SkipReason::Unreadable(
            "missing [CONNECTION] section".to_string(),
        ));
    }

    let protocol_name = ini.value("CONNECTION", "Protocol").unwrap_or("SSH");
    let protocol = match protocol_name.to_ascii_uppercase().as_str() {
        "" | "SSH" | "SSH1" | "SSH2" => ImportProtocol::Ssh,
        "TELNET" => ImportProtocol::Telnet,
        other => return Err(SkipReason::UnsupportedProtocol(other.to_string())),
    };

    let address = ini
        .value("CONNECTION", "Host")
        .or_else(|| ini.value("CONNECTION", "Address"))
        .unwrap_or("")
        .trim()
        .to_string();
    if address.is_empty() {
        return Err(SkipReason::MissingAddress);
    }

    let port = ini
        .value("CONNECTION", "Port")
        .and_then(|port| port.trim().parse::<u16>().ok())
        .filter(|port| *port != 0)
        .unwrap_or(match protocol {
            ImportProtocol::Ssh => super::SSH_PORT,
            ImportProtocol::Telnet => 23,
        });

    let user = ini
        .value("CONNECTION:AUTHENTICATION", "UserName")
        .or_else(|| ini.value("CONNECTION:AUTHENTICATION", "User"))
        .unwrap_or("")
        .trim()
        .to_string();

    let password = ini
        .value("CONNECTION:AUTHENTICATION", "Password")
        .unwrap_or("")
        .trim()
        .to_string();
    let passphrase = ini
        .value("CONNECTION:AUTHENTICATION", "Passphrase")
        .unwrap_or("")
        .trim()
        .to_string();
    let key_name = ini
        .value("CONNECTION:AUTHENTICATION", "UserKey")
        .unwrap_or("")
        .trim()
        .to_string();
    let methods: Vec<String> = ini
        .value("CONNECTION:AUTHENTICATION", "AuthMethodList")
        .unwrap_or("")
        .split(',')
        .map(|method| method.trim().to_string())
        .filter(|method| !method.is_empty())
        .collect();
    let asks_password = methods
        .iter()
        .any(|method| method == "01" || method == "10");
    // `UserKey` may be left over from an earlier setup; it only plays a
    // part when the session actually tries public key auth.
    let uses_key = methods.is_empty() || methods.iter().any(|method| method == "11");

    let identity = if protocol == ImportProtocol::Ssh && uses_key {
        resolve_key(base, &key_name)
    } else {
        None
    };
    let missing_key = if protocol == ImportProtocol::Ssh
        && uses_key
        && !key_name.is_empty()
        && identity.is_none()
    {
        Some(key_name.clone())
    } else {
        None
    };

    let needs_password = match protocol {
        // Telnet 的密码写在会话里，但和 SSH 一样是加密的；没有密码时
        // 让终端自己弹登录提示，不强行当自动登录。
        ImportProtocol::Telnet => !password.is_empty(),
        ImportProtocol::Ssh => {
            identity.is_none() && (!password.is_empty() || methods.is_empty() || asks_password)
        }
    };

    let mut discarded = Vec::new();
    if !passphrase.is_empty() {
        push_discarded(&mut discarded, DiscardedField::Passphrase);
    }
    if has_login_script(&ini) {
        push_discarded(&mut discarded, DiscardedField::LoginScript);
    }
    if ini
        .value("CONNECTION:SSH", "RemoteCommand")
        .map(|command| !command.trim().is_empty())
        .unwrap_or(false)
    {
        push_discarded(&mut discarded, DiscardedField::RemoteCommand);
    }
    if has_port_forwarding(&ini) {
        push_discarded(&mut discarded, DiscardedField::PortForwarding);
    }
    if has_proxy(&ini) {
        push_discarded(&mut discarded, DiscardedField::Proxy);
    }
    if has_terminal_settings(&ini) {
        push_discarded(&mut discarded, DiscardedField::Settings);
    }

    let notes = ini
        .value("CONNECTION", "Description")
        .unwrap_or("")
        .trim()
        .to_string();

    let mut candidate = ImportCandidate::ssh(name, address, port, user);
    candidate.identity = identity;
    candidate.group = group;
    candidate.protocol = protocol;
    candidate.needs_password = needs_password;
    candidate.password_cipher = (!password.is_empty()).then_some(password);
    candidate.missing_key = missing_key;
    candidate.notes = notes;
    candidate.discarded = discarded;
    Ok(candidate)
}

fn push_discarded(discarded: &mut Vec<DiscardedField>, field: DiscardedField) {
    if !discarded.contains(&field) {
        discarded.push(field);
    }
}

fn has_login_script(ini: &Ini) -> bool {
    ini.value("CONNECTION:AUTHENTICATION", "UseInitScript")
        .map(|value| value.trim() == "1")
        .unwrap_or(false)
        || ini
            .value("CONNECTION:AUTHENTICATION", "ScriptPath")
            .map(|path| !path.trim().is_empty())
            .unwrap_or(false)
        || ini
            .value("CONNECTION:AUTHENTICATION", "UseExpectSend")
            .map(|value| value.trim() == "1")
            .unwrap_or(false)
        || ini
            .value("CONNECTION:AUTHENTICATION", "ExpectSend_Count")
            .and_then(|count| count.trim().parse::<usize>().ok())
            .map(|count| count > 0)
            .unwrap_or(false)
}

fn has_port_forwarding(ini: &Ini) -> bool {
    ini.value("CONNECTION:SSH", "FwdReqCount")
        .and_then(|count| count.trim().parse::<usize>().ok())
        .map(|count| count > 0)
        .unwrap_or(false)
        || ini.any_key(|section, key| {
            section.contains("TUNNEL")
                || (key.starts_with("FWDREQ") && key != "FWDREQCOUNT")
                || key.starts_with("TUNNEL")
        })
}

fn has_proxy(ini: &Ini) -> bool {
    ini.value("CONNECTION:PROXY", "Proxy")
        .or_else(|| ini.value("CONNECTION", "Proxy"))
        .map(|proxy| !proxy.trim().is_empty())
        .unwrap_or(false)
}

fn has_terminal_settings(ini: &Ini) -> bool {
    let code_page = ini.value("TERMINAL", "CodePage").unwrap_or("").trim();
    let terminal_type = ini.value("TERMINAL", "Type").unwrap_or("").trim();
    let initial_directory = ini
        .value("CONNECTION:SSH", "InitRemoteDirectory")
        .unwrap_or("")
        .trim();
    (!code_page.is_empty() && code_page != "65001")
        || (!terminal_type.is_empty()
            && !terminal_type.eq_ignore_ascii_case("xterm")
            && !terminal_type.eq_ignore_ascii_case("xterm-256color"))
        || !initial_directory.is_empty()
}

fn resolve_key(base: Option<&Path>, key_name: &str) -> Option<PathBuf> {
    let key_name = key_name.trim();
    if key_name.is_empty() {
        return None;
    }
    let direct = PathBuf::from(key_name);
    if direct.is_file() {
        return Some(direct);
    }
    let base = base?;
    let directory = base.join("SECSH").join("UserKeys");
    let names = [key_name.to_string(), format!("{key_name}.pri")];
    for name in &names {
        let path = directory.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    let entries = fs::read_dir(&directory).ok()?;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&file_name))
            && entry.path().is_file()
        {
            return Some(entry.path());
        }
    }
    None
}

/// `.xsh` 的文本编码：BOM 优先，其次是 UTF-8，最后按 Xshell 在中文
/// Windows 上常用的 GBK 读。
fn decode_text(bytes: &[u8]) -> Option<String> {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return Some(String::from_utf8_lossy(rest).into_owned());
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, false);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text.to_string()),
        Err(_) => Some(GBK.decode(bytes).0.into_owned()),
    }
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Option<String> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let (pairs, _) = bytes.as_chunks::<2>();
    let units: Vec<u16> = pairs
        .iter()
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes(*pair)
            } else {
                u16::from_be_bytes(*pair)
            }
        })
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// `.xts` 的 zip 条目名：带 UTF-8 标记时是 UTF-8，否则是 GBK。
fn decode_entry_name(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(name) => name.to_string(),
        Err(_) => GBK.decode(raw).0.into_owned(),
    }
}

/// 从 zip 条目路径得到分组和文件名；不是 `.xsh` 或越出根目录的条目跳过。
fn archive_entry(name: &str) -> Option<(String, String)> {
    let normalized = name.replace('\\', "/");
    let mut parts: Vec<&str> = normalized
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.contains(&"..") {
        return None;
    }
    let file_name = parts.pop()?.to_string();
    if !file_name.to_ascii_lowercase().ends_with(".xsh") {
        return None;
    }
    if parts
        .first()
        .map(|part| part.eq_ignore_ascii_case("Xshell"))
        .unwrap_or(false)
    {
        parts.remove(0);
    }
    if parts
        .first()
        .map(|part| part.eq_ignore_ascii_case("Sessions"))
        .unwrap_or(false)
    {
        parts.remove(0);
    }
    Some((parts.join("/"), file_name))
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|extension| extension.to_ascii_lowercase())
}

fn display_name(path: &Path) -> String {
    path.file_stem()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn relative_group(root: &Path, file: &Path) -> String {
    let relative = file.strip_prefix(root).unwrap_or(file);
    let mut parts = Vec::new();
    let Some(parent) = relative.parent() else {
        return String::new();
    };
    for component in parent.components() {
        if let Component::Normal(name) = component {
            let name = name.to_string_lossy();
            if !name.is_empty() {
                parts.push(name.into_owned());
            }
        }
    }
    parts.join("/")
}

/// 单独选一个 `.xsh` 时，如果它在 `Sessions` 目录下，仍按它所在的目录
/// 还原分组。
fn single_group(path: &Path) -> String {
    let base = path
        .ancestors()
        .skip(1)
        .find(|ancestor| is_sessions_dir(ancestor));
    base.map(|sessions| relative_group(sessions, path))
        .unwrap_or_default()
}

/// `.xsh` 的 `UserKey` 是 Xshell 的密钥名，文件在 `SECSH/UserKeys`；
/// 从 `Sessions` 往上找到 Xshell 的版本目录。
fn xshell_base(path: &Path) -> Option<PathBuf> {
    let start = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    if let Some(sessions) = start.ancestors().find(|ancestor| is_sessions_dir(ancestor)) {
        return sessions.parent().map(Path::to_path_buf);
    }
    default_sessions_dir().parent().map(Path::to_path_buf)
}

fn is_sessions_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .map(|name| name.eq_ignore_ascii_case("Sessions"))
        .unwrap_or(false)
}

struct Ini {
    sections: HashMap<String, HashMap<String, String>>,
}

impl Ini {
    fn parse(text: &str) -> Self {
        let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut current = String::new();
        for line in text.lines() {
            let line = line.trim_start_matches('\u{feff}').trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            if let Some(section) = line
                .strip_prefix('[')
                .and_then(|line| line.strip_suffix(']'))
            {
                current = section.trim().to_ascii_uppercase();
                sections.entry(current.clone()).or_default();
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                sections
                    .entry(current.clone())
                    .or_default()
                    .insert(key.trim().to_ascii_uppercase(), value.trim().to_string());
            }
        }
        Self { sections }
    }

    fn has_section(&self, section: &str) -> bool {
        self.sections.contains_key(&section.to_ascii_uppercase())
    }

    fn value(&self, section: &str, key: &str) -> Option<&str> {
        self.sections
            .get(&section.to_ascii_uppercase())
            .and_then(|section| section.get(&key.to_ascii_uppercase()))
            .map(String::as_str)
    }

    fn any_key(&self, predicate: impl Fn(&str, &str) -> bool) -> bool {
        self.sections.iter().any(|(section, keys)| {
            keys.iter()
                .any(|(key, value)| !value.is_empty() && predicate(section, key))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;

    use tempfile::tempdir;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::host::import::ImportLoad;

    fn write_xsh(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut bytes = vec![0xFF, 0xFE];
        for unit in body.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        fs::write(path, bytes).unwrap();
    }

    fn session(host: &str, extra: &str) -> String {
        format!(
            "[CONNECTION]\nHost={host}\nPort=22\nProtocol=SSH\nDescription={}\n[CONNECTION:AUTHENTICATION]\n{extra}",
            "生产机"
        )
    }

    fn loaded(load: ImportLoad) -> ImportScan {
        match load {
            ImportLoad::Loaded(scan) => scan,
            other => panic!("expected a loaded scan, got {other:?}"),
        }
    }

    #[test]
    fn reads_a_session_tree_and_keeps_its_folders() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("Sessions");
        write_xsh(
            &sessions.join("生产/南海/web-01.xsh"),
            &session(
                "10.0.0.11",
                "UserName=deploy\nPassword=ZW5jcnlwdGVk\nAuthMethodList=01,10,20,30,40,50\n",
            ),
        );
        let scan = loaded(scan(&sessions));
        assert!(scan.skipped.is_empty());
        assert_eq!(scan.candidates.len(), 1);
        let candidate = &scan.candidates[0];
        assert_eq!(candidate.name, "web-01");
        assert_eq!(candidate.group, "生产/南海");
        assert_eq!(candidate.address, "10.0.0.11");
        assert_eq!(candidate.port, 22);
        assert_eq!(candidate.user, "deploy");
        assert_eq!(candidate.notes, "生产机");
        assert!(candidate.needs_password);
        assert!(candidate.discarded.is_empty());
    }

    #[test]
    fn a_missing_key_is_reported_and_the_file_still_imports() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("Sessions");
        write_xsh(
            &sessions.join("web.xsh"),
            &session(
                "10.0.0.12",
                "UserName=deploy\nUserKey=id_missing\nAuthMethodList=11\n",
            ),
        );
        let scan = loaded(scan(&sessions));
        let candidate = &scan.candidates[0];
        assert!(candidate.identity.is_none());
        assert_eq!(candidate.missing_key.as_deref(), Some("id_missing"));
        assert!(!candidate.needs_password);
    }

    #[test]
    fn a_key_in_the_xshell_key_folder_is_used() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("Sessions");
        let key = dir.path().join("SECSH/UserKeys/id_test.pri");
        fs::create_dir_all(key.parent().unwrap()).unwrap();
        fs::write(&key, "key").unwrap();
        write_xsh(
            &sessions.join("web.xsh"),
            &session(
                "10.0.0.13",
                "UserName=deploy\nUserKey=id_test\nAuthMethodList=11\n",
            ),
        );
        let scan = loaded(scan(&sessions));
        let candidate = &scan.candidates[0];
        assert_eq!(candidate.identity.as_deref(), Some(key.as_path()));
        assert!(candidate.missing_key.is_none());
    }

    #[test]
    fn telnet_keeps_its_protocol_and_default_port() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("Sessions");
        write_xsh(
            &sessions.join("switch.xsh"),
            "[CONNECTION]\nHost=10.0.0.20\nProtocol=TELNET\n[CONNECTION:AUTHENTICATION]\nUserName=admin\nPassword=ZW5jcnlwdGVk\n",
        );
        let scan = loaded(scan(&sessions));
        let candidate = &scan.candidates[0];
        assert_eq!(candidate.protocol, ImportProtocol::Telnet);
        assert_eq!(candidate.port, 23);
        assert!(candidate.needs_password);
    }

    #[test]
    fn unsupported_protocols_and_broken_files_are_skipped_not_fatal() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("Sessions");
        write_xsh(
            &sessions.join("serial.xsh"),
            "[CONNECTION]\nHost=COM7\nProtocol=SERIAL\n",
        );
        write_xsh(
            &sessions.join("no-address.xsh"),
            "[CONNECTION]\nProtocol=SSH\n",
        );
        fs::write(sessions.join("broken.xsh"), b"not an ini file").unwrap();
        write_xsh(&sessions.join("ok.xsh"), &session("10.0.0.14", ""));

        let scan = loaded(scan(&sessions));
        assert_eq!(scan.candidates.len(), 1);
        assert_eq!(scan.candidates[0].address, "10.0.0.14");
        assert_eq!(scan.skipped.len(), 3);
        assert!(
            scan.skipped
                .iter()
                .any(|skipped| matches!(skipped.reason, SkipReason::UnsupportedProtocol(_)))
        );
        assert!(
            scan.skipped
                .iter()
                .any(|skipped| matches!(skipped.reason, SkipReason::MissingAddress))
        );
        assert!(
            scan.skipped
                .iter()
                .any(|skipped| matches!(skipped.reason, SkipReason::Unreadable(_)))
        );
    }

    #[test]
    fn login_scripts_forwarding_and_proxies_are_reported_as_discarded() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("Sessions");
        write_xsh(
            &sessions.join("web.xsh"),
            &format!(
                "{}\n[CONNECTION:SSH]\nFwdReqCount=1\nRemoteCommand=uptime\n[CONNECTION:PROXY]\nProxy=office\n",
                session(
                    "10.0.0.15",
                    "UserName=deploy\nUseInitScript=1\nScriptPath=login.vbs\n"
                )
            ),
        );
        let scan = loaded(scan(&sessions));
        let candidate = &scan.candidates[0];
        assert!(candidate.discarded.contains(&DiscardedField::LoginScript));
        assert!(candidate.discarded.contains(&DiscardedField::RemoteCommand));
        assert!(
            candidate
                .discarded
                .contains(&DiscardedField::PortForwarding)
        );
        assert!(candidate.discarded.contains(&DiscardedField::Proxy));
    }

    #[test]
    fn a_single_xsh_file_still_finds_its_folder() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("Sessions/公司/生产/web.xsh");
        write_xsh(&path, &session("10.0.0.16", ""));
        let scan = loaded(scan(&path));
        assert_eq!(scan.candidates[0].group, "公司/生产");
    }

    #[test]
    fn reads_an_xts_archive() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.xts");
        let file = fs::File::create(&path).unwrap();
        let mut writer = ZipWriter::new(file);
        let mut body = vec![0xFF, 0xFE];
        for unit in session("10.0.0.17", "").encode_utf16() {
            body.extend_from_slice(&unit.to_le_bytes());
        }
        writer
            .start_file("Xshell/团队/生产/web.xsh", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(&body).unwrap();
        writer.finish().unwrap();

        let scan = loaded(scan(&path));
        assert_eq!(scan.candidates.len(), 1);
        assert_eq!(scan.candidates[0].group, "团队/生产");
    }

    #[test]
    fn archive_names_are_decoded_from_gbk_when_needed() {
        let raw = GBK.encode("团队").0;
        assert_eq!(decode_entry_name(&raw), "团队");
    }

    #[test]
    fn archive_entries_outside_the_tree_are_dropped() {
        assert!(archive_entry("../evil.xsh").is_none());
        assert!(archive_entry("Xshell/default.xfs").is_none());
        assert_eq!(
            archive_entry("Xshell/团队/web.xsh"),
            Some(("团队".to_string(), "web.xsh".to_string()))
        );
    }

    #[test]
    fn gbk_sessions_are_read_too() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("Sessions/web.xsh");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            b"[CONNECTION]\nHost=10.0.0.18\nProtocol=SSH\nDescription=\xd6\xd0\xce\xc4\n",
        )
        .unwrap();
        let scan = loaded(scan(&path));
        assert_eq!(scan.candidates[0].notes, "中文");
    }

    #[test]
    fn missing_paths_are_reported_as_missing() {
        let dir = tempdir().unwrap();
        assert_eq!(scan(&dir.path().join("nope")), ImportLoad::Missing);
    }

    #[test]
    fn a_non_session_file_is_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        fs::write(&path, "hello").unwrap();
        assert!(matches!(scan(&path), ImportLoad::Failed(_)));
    }
}
