//! The channels a live connection opens: the shell on a pseudo-terminal, a
//! command run beside it, and SFTP. Each fails with a [`ChannelError`].

use std::time::Duration;

use anyhow::{Result, anyhow};
use russh::{Channel, ChannelMsg, client};
use tokio::io::{AsyncRead, AsyncWrite};

use super::connection::SshHandle;
use super::error::ChannelError;

/// A byte stream on a connection, for a protocol beside the shell, or the
/// way through a jump host or proxy.
pub trait ChannelStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> ChannelStream for T {}

/// An SFTP channel on a connection, and the fingerprint of the host key that
/// connection checked, which resumed transfers verify.
pub struct SftpChannel {
    pub stream: Box<dyn ChannelStream>,
    pub fingerprint: String,
}

/// A terminal's size, in cells and in pixels. Pixels may be 0 when unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PtySize {
    pub columns: u32,
    pub rows: u32,
    pub width: u32,
    pub height: u32,
}

/// Open a session channel and start the user's shell on a pseudo-terminal
/// of type `term`.
pub async fn open_shell(
    handle: &SshHandle,
    term: &str,
    size: PtySize,
) -> Result<Channel<client::Msg>> {
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|_| anyhow!(ChannelError::SessionFailed))?;
    channel
        .request_pty(
            true,
            term,
            size.columns,
            size.rows,
            size.width,
            size.height,
            &[],
        )
        .await
        .map_err(|_| anyhow!(ChannelError::PtyRefused))?;
    channel
        .request_shell(true)
        .await
        .map_err(|_| anyhow!(ChannelError::ShellRefused))?;
    Ok(channel)
}

/// Tell the shell its terminal changed size.
pub async fn resize_pty(channel: &Channel<client::Msg>, size: PtySize) -> Result<()> {
    channel
        .window_change(size.columns, size.rows, size.width, size.height)
        .await
        .map_err(|_| anyhow!(ChannelError::ResizeFailed))
}

/// Type `bytes` into the shell.
pub async fn write_shell(channel: &Channel<client::Msg>, bytes: Vec<u8>) -> Result<()> {
    channel
        .data_bytes(bytes)
        .await
        .map_err(|_| anyhow!(ChannelError::WriteFailed))
}

/// Run `command` on a channel of its own and give back what it printed on
/// standard output, at most `limit` bytes of it; its standard error is
/// dropped. Gives up after `timeout`.
pub async fn run_exec(
    handle: &SshHandle,
    command: &str,
    limit: usize,
    timeout: Duration,
) -> Result<Vec<u8>> {
    tokio::time::timeout(timeout, async {
        let mut channel = handle
            .channel_open_session()
            .await
            .map_err(|_| anyhow!(ChannelError::ExecChannelFailed))?;
        channel
            .exec(true, command)
            .await
            .map_err(|_| anyhow!(ChannelError::ExecSendFailed))?;
        let mut output = Vec::new();
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    let room = limit.saturating_sub(output.len());
                    output.extend_from_slice(&data[..data.len().min(room)]);
                }
                Some(ChannelMsg::Failure) => return Err(anyhow!(ChannelError::ExecRefused)),
                Some(ChannelMsg::Eof | ChannelMsg::Close) | None => break,
                _ => {}
            }
        }
        Ok(output)
    })
    .await
    .unwrap_or_else(|_| Err(anyhow!(ChannelError::ExecTimeout)))
}

/// What one command printed when it was run with its streams kept apart:
/// both of them, up to a shared limit, its exit code when the server sent
/// one, and whether it was cut short.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecDetail {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<u32>,
    /// `limit` was reached: what came after was dropped.
    pub truncated: bool,
    /// `timeout` ran out before the command ended; what it printed before
    /// that is still here.
    pub timed_out: bool,
}

/// Run `command` on a channel of its own and give back both streams and the
/// exit code, keeping at most `limit` bytes across the two. A command that
/// outlives `timeout` keeps what it printed until then, marked
/// [`ExecDetail::timed_out`].
pub async fn run_exec_detail(
    handle: &SshHandle,
    command: &str,
    limit: usize,
    timeout: Duration,
) -> Result<ExecDetail> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut channel = tokio::time::timeout_at(deadline, handle.channel_open_session())
        .await
        .map_err(|_| anyhow!(ChannelError::ExecTimeout))?
        .map_err(|_| anyhow!(ChannelError::ExecChannelFailed))?;
    tokio::time::timeout_at(deadline, channel.exec(true, command))
        .await
        .map_err(|_| anyhow!(ChannelError::ExecTimeout))?
        .map_err(|_| anyhow!(ChannelError::ExecSendFailed))?;

    let mut detail = ExecDetail::default();
    let mut room = limit;
    loop {
        match tokio::time::timeout_at(deadline, channel.wait()).await {
            Err(_) => {
                detail.timed_out = true;
                break;
            }
            Ok(Some(ChannelMsg::Data { data })) => {
                push_capped(&mut detail.stdout, &data, &mut room, &mut detail.truncated)
            }
            Ok(Some(ChannelMsg::ExtendedData { data, .. })) => {
                push_capped(&mut detail.stderr, &data, &mut room, &mut detail.truncated)
            }
            Ok(Some(ChannelMsg::ExitStatus { exit_status })) => {
                detail.exit_code = Some(exit_status)
            }
            Ok(Some(ChannelMsg::Failure)) => return Err(anyhow!(ChannelError::ExecRefused)),
            Ok(Some(ChannelMsg::Eof | ChannelMsg::Close)) | Ok(None) => break,
            Ok(Some(_)) => {}
        }
    }
    Ok(detail)
}

/// Keep what `room` allows of one chunk and say when it was cut.
fn push_capped(target: &mut Vec<u8>, data: &[u8], room: &mut usize, truncated: &mut bool) {
    let kept = data.len().min(*room);
    target.extend_from_slice(&data[..kept]);
    *room -= kept;
    if kept < data.len() {
        *truncated = true;
    }
}

/// Open an SFTP channel. Gives up after `timeout`.
pub async fn open_sftp_channel(
    handle: &SshHandle,
    timeout: Duration,
) -> Result<Box<dyn ChannelStream>> {
    tokio::time::timeout(timeout, async {
        let mut channel = handle
            .channel_open_session()
            .await
            .map_err(|_| anyhow!(ChannelError::ExecChannelFailed))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|_| anyhow!(ChannelError::SftpRefused))?;
        // russh sends the request without waiting for the answer: wait here,
        // so a server without SFTP says so rather than ending the stream,
        // which would read as a dropped connection worth retrying.
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Success) => break,
                Some(ChannelMsg::Failure | ChannelMsg::Eof | ChannelMsg::Close) | None => {
                    return Err(anyhow!(ChannelError::SftpRefused));
                }
                _ => {}
            }
        }
        Ok(Box::new(channel.into_stream()) as Box<dyn ChannelStream>)
    })
    .await
    .unwrap_or_else(|_| Err(anyhow!(ChannelError::SftpTimeout)))
}

/// Open a channel and run one probe command on it (see
/// [`HostOsProbe`](super::probe::HostOsProbe)). Best effort throughout: a
/// server that refuses the channel or the command gives `None`.
pub async fn open_probe(
    handle: &SshHandle,
    command: Option<&'static str>,
) -> Option<Channel<client::Msg>> {
    let command = command?;
    let channel = handle.channel_open_session().await.ok()?;
    // No reply wanted: the answer is the output, and waiting for the
    // acknowledgement would cost another round trip for nothing.
    channel.exec(false, command).await.ok()?;
    Some(channel)
}

#[cfg(test)]
mod tests {
    use super::push_capped;

    #[test]
    fn chunks_share_one_budget_and_the_cut_is_marked() {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let mut room = 6;
        let mut truncated = false;
        push_capped(&mut stdout, b"abcd", &mut room, &mut truncated);
        assert_eq!(stdout, b"abcd");
        assert!(!truncated);
        push_capped(&mut stderr, b"efgh", &mut room, &mut truncated);
        assert_eq!(stderr, b"ef");
        assert!(truncated, "the chunk was cut");
        assert_eq!(room, 0);
        push_capped(&mut stdout, b"i", &mut room, &mut truncated);
        assert_eq!(stdout, b"abcd", "nothing is kept once the budget is spent");
        assert!(truncated);
    }

    #[test]
    fn output_under_the_budget_is_kept_whole() {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let mut room = 1024;
        let mut truncated = false;
        push_capped(&mut stdout, b"hello\n", &mut room, &mut truncated);
        push_capped(&mut stderr, b"warn\n", &mut room, &mut truncated);
        assert_eq!(stdout, b"hello\n");
        assert_eq!(stderr, b"warn\n");
        assert!(!truncated);
        assert_eq!(room, 1024 - 11);
    }
}
