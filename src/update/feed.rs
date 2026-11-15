//! Fetching releases and packages.
//!
//! Every method blocks and runs on an update worker thread. The feed only
//! moves bytes: whether to believe them is `verify`'s job, so a feed cannot
//! skip the check, and the fake one in the tests goes through it too.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, CONTENT_RANGE, RANGE};

use super::build_info::{self, Channel};
use super::error::UpdateError;
use super::platform;
use crate::i18n::t;

/// Where releases and packages come from.
pub trait UpdateFeed: Send + Sync {
    /// What the server says about `channel`: a signed envelope, or a GitHub
    /// release description.
    fn fetch(&self, channel: Channel) -> Result<Vec<u8>, UpdateError>;

    /// Download a package of `size` bytes into `dest`, trying `urls` in
    /// order. Bytes already in `dest`'s `.part` file are kept and the rest
    /// is asked for with a range. `progress` hears the bytes on disk so far.
    fn download(
        &self,
        urls: &[String],
        size: u64,
        dest: &Path,
        progress: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
    ) -> Result<(), UpdateError>;
}

/// Where a download in progress is kept: next to its destination, so that
/// finishing it is a rename.
pub fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

/// The largest release description believed to be one.
const RELEASE_LIMIT: u64 = 1024 * 1024;

/// The real feed, over HTTPS.
pub struct HttpFeed {
    /// The releases' address; a channel appends `/latest` or `?per_page=20`.
    releases_url: String,
    /// How long a connection may stay silent, while connecting, waiting for
    /// an answer or in the middle of a download.
    patience: Duration,
    /// Whether to go through the system's proxy. Only the loopback tests turn
    /// it off: a developer's proxy would capture 127.0.0.1.
    system_proxy: bool,
    /// Whether a request must use HTTPS. Only the loopback tests turn it off.
    secure: bool,
}

impl HttpFeed {
    /// The feed release builds use. A debug build can be pointed at a local
    /// server with `SHELLRS_UPDATE_URL`; a release build cannot, so nothing
    /// in the environment decides where the app updates itself from.
    pub fn system() -> Self {
        let override_url = cfg!(debug_assertions)
            .then(|| std::env::var("SHELLRS_UPDATE_URL").ok())
            .flatten();
        match override_url {
            Some(url) => Self {
                // A developer's local server does not speak HTTPS.
                secure: false,
                ..Self::new(url)
            },
            None => Self::new(build_info::RELEASES_API.to_string()),
        }
    }

    pub fn new(releases_url: String) -> Self {
        Self {
            releases_url: releases_url.trim_end_matches('/').to_string(),
            patience: Duration::from_secs(30),
            system_proxy: true,
            secure: true,
        }
    }

    /// Built on the worker thread, per job: the blocking client runs a
    /// runtime of its own, which must not be created or dropped inside an
    /// async context.
    fn client(&self) -> Result<Client, UpdateError> {
        let builder = Client::builder()
            .https_only(self.secure)
            .user_agent(platform::user_agent(build_info::VERSION))
            .connect_timeout(self.patience)
            // Per read, in the blocking client: a download that stops
            // moving fails after this long, a slow one keeps going.
            .timeout(self.patience);
        let builder = if self.system_proxy {
            builder
        } else {
            builder.no_proxy()
        };
        builder
            .build()
            .map_err(|error| UpdateError::Network(describe(&error)))
    }

    fn download_from(
        &self,
        client: &Client,
        url: &str,
        size: u64,
        part: &Path,
        progress: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
    ) -> Result<(), UpdateError> {
        let mut have = fs::metadata(part).map(|meta| meta.len()).unwrap_or(0);
        if have > size {
            fs::remove_file(part)?;
            have = 0;
        }
        if have == size {
            return Ok(());
        }
        let mut request = client.get(url);
        if have > 0 {
            request = request.header(RANGE, format!("bytes={have}-"));
        }
        let mut response = request
            .send()
            .map_err(|error| UpdateError::Network(describe(&error)))?;
        let mut file = match response.status() {
            StatusCode::PARTIAL_CONTENT if have > 0 => {
                if range_start(&response) != Some(have) {
                    // Not the bytes that were asked for: start over next time.
                    fs::remove_file(part)?;
                    return Err(UpdateError::Network(
                        t!("update.feed.wrong_range").to_string(),
                    ));
                }
                OpenOptions::new().append(true).open(part)?
            }
            // The whole file, also from a server that ignores ranges.
            StatusCode::OK => {
                have = 0;
                File::create(part)?
            }
            status => return Err(UpdateError::Http(status.as_u16())),
        };
        progress(have);
        let mut buffer = vec![0; 64 * 1024];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(UpdateError::Cancelled);
            }
            let read = response.read(&mut buffer).map_err(read_error)?;
            if read == 0 {
                break;
            }
            have += read as u64;
            if have > size {
                drop(file);
                fs::remove_file(part)?;
                return Err(UpdateError::Corrupt);
            }
            file.write_all(&buffer[..read]).map_err(write_error)?;
            progress(have);
        }
        file.sync_all().map_err(write_error)?;
        if have < size {
            return Err(UpdateError::Network(t!("update.feed.cut_off").to_string()));
        }
        Ok(())
    }
}

impl UpdateFeed for HttpFeed {
    fn fetch(&self, channel: Channel) -> Result<Vec<u8>, UpdateError> {
        let client = self.client()?;
        // A development build may still point at a server that serves one
        // file per channel.
        let url = if self.releases_url.contains("{channel}") {
            self.releases_url.replace("{channel}", channel.key())
        } else {
            match channel {
                Channel::Stable => format!("{}/latest", self.releases_url),
                Channel::Beta => format!("{}?per_page=20", self.releases_url),
            }
        };
        let response = client
            .get(url)
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .map_err(|error| UpdateError::Network(describe(&error)))?;
        let status = response.status();
        if !status.is_success() {
            let rate_limited = response
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|value| value.to_str().ok())
                == Some("0");
            return Err(match status.as_u16() {
                403 | 429 if rate_limited || status == StatusCode::TOO_MANY_REQUESTS => {
                    UpdateError::RateLimited
                }
                404 => UpdateError::NotFound,
                code => UpdateError::Http(code),
            });
        }
        let mut body = Vec::new();
        response
            .take(RELEASE_LIMIT + 1)
            .read_to_end(&mut body)
            .map_err(read_error)?;
        if body.len() as u64 > RELEASE_LIMIT {
            return Err(UpdateError::BadManifest(
                t!("update.feed.too_large").to_string(),
            ));
        }
        Ok(body)
    }

    fn download(
        &self,
        urls: &[String],
        size: u64,
        dest: &Path,
        progress: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
    ) -> Result<(), UpdateError> {
        let client = self.client()?;
        let part = part_path(dest);
        let mut last = UpdateError::Network(t!("update.feed.no_urls").to_string());
        for url in urls {
            match self.download_from(&client, url, size, &part, progress, cancel) {
                Ok(()) => {
                    fs::rename(&part, dest)?;
                    return Ok(());
                }
                Err(
                    error @ (UpdateError::Cancelled | UpdateError::Disk(_) | UpdateError::NoSpace),
                ) => {
                    return Err(error);
                }
                // The next address may be a mirror that works; what is on
                // disk is kept, the checksum decides in the end.
                Err(error) => last = error,
            }
        }
        Err(last)
    }
}

/// The first byte of a `206` answer, from `Content-Range: bytes 10-99/100`.
fn range_start(response: &Response) -> Option<u64> {
    let value = response.headers().get(CONTENT_RANGE)?.to_str().ok()?;
    let range = value.strip_prefix("bytes ")?;
    range.split('-').next()?.trim().parse().ok()
}

/// A failed read of a response body. A timeout arrives as reqwest's own
/// error wrapped in an `io::Error`, worded as a decoding error.
fn read_error(error: io::Error) -> UpdateError {
    let timed_out = error.kind() == io::ErrorKind::TimedOut
        || error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<reqwest::Error>())
            .is_some_and(reqwest::Error::is_timeout);
    if timed_out {
        return UpdateError::Stalled;
    }
    match error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<reqwest::Error>())
    {
        Some(inner) => UpdateError::Network(describe(inner)),
        None => UpdateError::Network(error.to_string()),
    }
}

/// A failed write, telling a full disk apart.
pub(super) fn write_error(error: io::Error) -> UpdateError {
    if error.kind() == io::ErrorKind::StorageFull {
        UpdateError::NoSpace
    } else {
        UpdateError::Disk(error.to_string())
    }
}

/// A request error in a few words: reqwest's own message only names the
/// URL, the reason is at the end of its chain.
fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        return t!("update.feed.timed_out").to_string();
    }
    let mut source: &dyn std::error::Error = error;
    while let Some(next) = source.source() {
        source = next;
    }
    source.to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use super::*;

    /// How the loopback server answers a path.
    #[derive(Clone)]
    pub(crate) enum Answer {
        /// The body, honouring ranges.
        Body(Vec<u8>),
        /// The whole body whatever the range.
        IgnoringRange(Vec<u8>),
        /// The headers of the whole body, then only its first bytes.
        CutAfter(Vec<u8>, usize),
        /// The headers, then nothing.
        Silence(Vec<u8>),
        Status(u16),
        /// GitHub's answer to too many requests in an hour.
        RateLimited,
    }

    /// A request as the server saw it.
    #[derive(Clone, Debug)]
    pub(crate) struct Seen {
        pub path: String,
        pub range: Option<String>,
        pub user_agent: Option<String>,
    }

    /// A small HTTP/1.1 server on 127.0.0.1, one connection per request.
    pub(crate) struct Server {
        port: u16,
        answers: Arc<Mutex<HashMap<String, Answer>>>,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl Server {
        pub(crate) fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let answers = Arc::new(Mutex::new(HashMap::new()));
            let seen = Arc::new(Mutex::new(Vec::new()));
            let (shared_answers, shared_seen) = (answers.clone(), seen.clone());
            thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let (answers, seen) = (shared_answers.clone(), shared_seen.clone());
                    thread::spawn(move || serve(stream, &answers, &seen));
                }
            });
            Self {
                port,
                answers,
                seen,
            }
        }

        pub(crate) fn url(&self, path: &str) -> String {
            format!("http://127.0.0.1:{}{path}", self.port)
        }

        pub(crate) fn answer(&self, path: &str, answer: Answer) {
            self.answers.lock().unwrap().insert(path.into(), answer);
        }

        pub(crate) fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }

        /// A feed reading this server's `/releases`.
        pub(crate) fn feed(&self) -> HttpFeed {
            HttpFeed {
                releases_url: self.url("/releases"),
                patience: Duration::from_millis(500),
                system_proxy: false,
                secure: false,
            }
        }
    }

    fn serve(stream: TcpStream, answers: &Mutex<HashMap<String, Answer>>, seen: &Mutex<Vec<Seen>>) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .to_string();
        let (mut range, mut user_agent) = (None, None);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                match name.trim().to_ascii_lowercase().as_str() {
                    "range" => range = Some(value.trim().to_string()),
                    "user-agent" => user_agent = Some(value.trim().to_string()),
                    _ => {}
                }
            }
        }
        seen.lock().unwrap().push(Seen {
            path: path.clone(),
            range: range.clone(),
            user_agent,
        });
        let answer = answers.lock().unwrap().get(&path).cloned();
        let mut stream = stream;
        let head = |status: &str, length: usize, extra: &str| {
            format!(
                "HTTP/1.1 {status}\r\nContent-Length: {length}\r\nConnection: close\r\n{extra}\r\n"
            )
        };
        let start = range
            .as_deref()
            .and_then(|range| range.strip_prefix("bytes="))
            .and_then(|range| range.trim_end_matches('-').parse::<usize>().ok());
        let _ = match answer {
            None | Some(Answer::Status(404)) => {
                stream.write_all(head("404 Not Found", 0, "").as_bytes())
            }
            Some(Answer::Status(code)) => {
                stream.write_all(head(&format!("{code} Whatever"), 0, "").as_bytes())
            }
            Some(Answer::RateLimited) => stream
                .write_all(head("403 Forbidden", 0, "x-ratelimit-remaining: 0\r\n").as_bytes()),
            Some(Answer::Body(body)) => match start {
                Some(start) if start < body.len() => {
                    let extra = format!(
                        "Content-Range: bytes {start}-{}/{}\r\n",
                        body.len() - 1,
                        body.len()
                    );
                    stream
                        .write_all(
                            head("206 Partial Content", body.len() - start, &extra).as_bytes(),
                        )
                        .and_then(|()| stream.write_all(&body[start..]))
                }
                _ => stream
                    .write_all(head("200 OK", body.len(), "").as_bytes())
                    .and_then(|()| stream.write_all(&body)),
            },
            Some(Answer::IgnoringRange(body)) => stream
                .write_all(head("200 OK", body.len(), "").as_bytes())
                .and_then(|()| stream.write_all(&body)),
            Some(Answer::CutAfter(body, cut)) => stream
                .write_all(head("200 OK", body.len(), "").as_bytes())
                .and_then(|()| stream.write_all(&body[..cut])),
            Some(Answer::Silence(body)) => stream
                .write_all(head("200 OK", body.len(), "").as_bytes())
                .map(|()| thread::sleep(Duration::from_secs(3))),
        };
    }

    fn package(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn download(
        feed: &HttpFeed,
        urls: &[String],
        size: u64,
        dest: &Path,
    ) -> (Result<(), UpdateError>, Vec<u64>) {
        let mut heard = Vec::new();
        let result = feed.download(
            urls,
            size,
            dest,
            &mut |bytes| heard.push(bytes),
            &AtomicBool::new(false),
        );
        (result, heard)
    }

    #[test]
    fn the_release_is_fetched_with_the_forks_user_agent() {
        let server = Server::start();
        server.answer(
            "/releases?per_page=20",
            Answer::Body(br#"{"tag_name":"v0.2.0"}"#.to_vec()),
        );
        assert_eq!(
            server.feed().fetch(Channel::Beta),
            Ok(br#"{"tag_name":"v0.2.0"}"#.to_vec())
        );
        // Nothing was published yet.
        assert_eq!(
            server.feed().fetch(Channel::Stable),
            Err(UpdateError::NotFound)
        );
        let seen = server.seen();
        assert_eq!(seen[0].path, "/releases?per_page=20");
        assert_eq!(seen[1].path, "/releases/latest");
        assert!(
            seen[0]
                .user_agent
                .as_deref()
                .is_some_and(|agent| agent.starts_with("AgreeShellAS/")),
            "{seen:?}"
        );
    }

    #[test]
    fn a_rate_limited_check_says_so() {
        let server = Server::start();
        server.answer("/releases/latest", Answer::RateLimited);
        assert_eq!(
            server.feed().fetch(Channel::Stable),
            Err(UpdateError::RateLimited)
        );
    }

    #[test]
    fn a_download_resumes_from_its_part_file() {
        let server = Server::start();
        let body = package(200_000);
        server.answer("/p", Answer::Body(body.clone()));
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("package.zip");
        fs::write(part_path(&dest), &body[..150_000]).unwrap();

        let (result, heard) = download(&server.feed(), &[server.url("/p")], 200_000, &dest);
        assert_eq!(result, Ok(()));
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert!(!part_path(&dest).exists());
        assert_eq!(heard.first(), Some(&150_000));
        assert_eq!(heard.last(), Some(&200_000));
        assert_eq!(server.seen()[0].range.as_deref(), Some("bytes=150000-"));
    }

    #[test]
    fn a_server_ignoring_range_starts_over() {
        let server = Server::start();
        let body = package(50_000);
        server.answer("/p", Answer::IgnoringRange(body.clone()));
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("package.zip");
        fs::write(part_path(&dest), &body[..10_000]).unwrap();

        let (result, _) = download(&server.feed(), &[server.url("/p")], 50_000, &dest);
        assert_eq!(result, Ok(()));
        assert_eq!(fs::read(&dest).unwrap(), body);
    }

    #[test]
    fn the_next_url_is_tried_when_one_fails() {
        let server = Server::start();
        let body = package(80_000);
        server.answer("/broken", Answer::CutAfter(body.clone(), 30_000));
        server.answer("/mirror", Answer::Body(body.clone()));
        server.answer("/busy", Answer::Status(503));
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("package.zip");

        let only_busy = download(&server.feed(), &[server.url("/busy")], 80_000, &dest);
        assert_eq!(only_busy.0, Err(UpdateError::Http(503)));

        let urls = [
            server.url("/missing"),
            server.url("/busy"),
            server.url("/broken"),
            server.url("/mirror"),
        ];
        let (result, _) = download(&server.feed(), &urls, 80_000, &dest);
        assert_eq!(result, Ok(()));
        assert_eq!(fs::read(&dest).unwrap(), body);
        // The mirror was asked only for what the broken address had not sent.
        let seen = server.seen();
        assert_eq!(seen.last().unwrap().path, "/mirror");
        assert_eq!(seen.last().unwrap().range.as_deref(), Some("bytes=30000-"));
    }

    #[test]
    fn a_stalled_download_fails_instead_of_hanging() {
        let server = Server::start();
        server.answer("/p", Answer::Silence(package(1000)));
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("package.zip");
        let (result, _) = download(&server.feed(), &[server.url("/p")], 1000, &dest);
        assert_eq!(result, Err(UpdateError::Stalled));
    }

    #[test]
    fn a_package_larger_than_promised_is_thrown_away() {
        let server = Server::start();
        server.answer("/p", Answer::Body(package(2000)));
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("package.zip");
        let (result, _) = download(&server.feed(), &[server.url("/p")], 1000, &dest);
        assert_eq!(result, Err(UpdateError::Corrupt));
        assert!(!part_path(&dest).exists());
        assert!(!dest.exists());
    }

    #[test]
    fn cancelling_stops_the_download() {
        let server = Server::start();
        server.answer("/p", Answer::Body(package(300_000)));
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("package.zip");
        let cancel = AtomicBool::new(false);
        let result = server.feed().download(
            &[server.url("/p")],
            300_000,
            &dest,
            &mut |bytes| {
                if bytes > 0 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
        );
        assert_eq!(result, Err(UpdateError::Cancelled));
        assert!(!dest.exists());
    }

    #[test]
    fn a_complete_part_file_needs_no_request() {
        let server = Server::start();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("package.zip");
        fs::write(part_path(&dest), package(10)).unwrap();
        let (result, _) = download(&server.feed(), &[server.url("/p")], 10, &dest);
        assert_eq!(result, Ok(()));
        assert!(server.seen().is_empty());
    }
}
