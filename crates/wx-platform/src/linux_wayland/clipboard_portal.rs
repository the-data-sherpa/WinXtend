//! The `org.freedesktop.portal.Clipboard` half of the clipboard.
//!
//! Everything here talks to a live desktop; the rules it serves live in
//! [`super::clipboard`], which needs none. The split is the same one
//! [`super::driver`] and [`super::session`] have, and for the same reason: this
//! crate compiles and tests on Windows and macOS with no session at all.
//!
//! # It is not a driver of its own
//!
//! There is no thread here. `RequestClipboard` is refused for anything that is
//! not a `RemoteDesktop` session, so the clipboard rides the session
//! [`super::driver`] already owns, on the same thread, in the same `select!` — a
//! second session would mean a second consent dialog for one machine, and the
//! portal would refuse it anyway.
//!
//! `RequestClipboard` must be called **after `SelectDevices` and before
//! `Start`**, because it is part of what the user is being asked to allow. It is
//! answered in `Start`'s results as `clipboard_enabled`, separately from the
//! devices, so a user can grant one and refuse the other.
//!
//! # Everything moves over a pipe
//!
//! Reading is `SelectionRead` → a file descriptor to drain. Being pasted *from*
//! is a `SelectionTransfer` signal → `SelectionWrite` → a file descriptor to
//! fill → `SelectionWriteDone`. Both are asynchronous and neither may block the
//! session's event loop: a 50 MiB image takes a few hundred milliseconds to move,
//! and a loop stalled on it would stop servicing injection, revocation and
//! shutdown for the duration. So every transfer is a task of its own and the loop
//! only routes.
//!
//! The synchronous [`crate::traits::ClipboardAccess`] side reaches this thread
//! through [`Requests`], which posts a command and blocks on a reply channel with
//! a timeout — because the thing at the other end of the pipe is another
//! application, and one that stops reading must not wedge the agent's clipboard
//! loop for the rest of the run.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use ashpd::desktop::clipboard::{
    Clipboard, RequestClipboardOptions, SelectionOwnerChanged, SetSelectionOptions,
};
use ashpd::desktop::remote_desktop::RemoteDesktop;
use ashpd::desktop::Session;
use futures_util::Stream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use super::clipboard::{ClipboardState, SelectionTransport, MAX_CLIPBOARD_BYTES};
use crate::error::{PlatformError, Result};

/// How long a trait call waits for the portal thread before giving up.
///
/// Not a performance budget: a `SelectionRead` answers in single-digit
/// milliseconds for small payloads and under 300 ms for 50 MiB on the alpha
/// target. It is the bound that stops an application which has stopped draining
/// its end of a pipe from wedging the agent's clipboard loop.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one transfer may take on the portal thread.
///
/// The same rule from the other side: a task waiting forever on a pipe nobody
/// reads is a task that never ends, and the runtime is joined at shutdown.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);

/// Chunk size for draining a selection pipe. Large enough that a 32 MiB image is
/// a few hundred reads rather than a few thousand.
const CHUNK: usize = 64 * 1024;

/// The signal streams borrow the proxy they came from.
///
/// `ashpd` hands them back as an `impl Stream` that captures `&self`, so the
/// proxy has to outlive them — which is why [`proxy`] is built in the session
/// thread's own frame and everything downstream borrows it, rather than each
/// piece making its own.
type OwnerChanges<'a> =
    Pin<Box<dyn Stream<Item = (Session<RemoteDesktop>, SelectionOwnerChanged)> + Send + 'a>>;
type Transfers<'a> = Pin<Box<dyn Stream<Item = (Session<RemoteDesktop>, String, u32)> + Send + 'a>>;

/// A clipboard that has been asked for and subscribed to.
///
/// Built between `SelectDevices` and `Start`. Whether it is worth anything
/// depends on what `Start` answers.
pub struct Portal<'a> {
    pub owner_changes: OwnerChanges<'a>,
    pub transfers: Transfers<'a>,
}

/// The `Clipboard` proxy, or `None` on a desktop that has no such portal.
///
/// Separate from [`request`] because the signal streams borrow it: it has to be
/// owned by something that outlives the whole session, and creating it costs
/// nothing and asks the user for nothing.
pub async fn proxy() -> Option<Arc<Clipboard>> {
    match Clipboard::new().await {
        Ok(proxy) => {
            tracing::debug!(version = proxy.version(), "portal Clipboard interface");
            Some(Arc::new(proxy))
        }
        Err(e) => {
            tracing::info!(error = %e, "this desktop has no clipboard portal; clipboard sync is unavailable");
            None
        }
    }
}

/// Ask for clipboard access on an existing session, and subscribe to its signals.
///
/// Returns `None` — with a reason in the log — rather than failing, because a
/// portal that refuses the request must still leave a working injection session.
/// The clipboard is a capability this backend either has or does not advertise;
/// it is never a reason to give back a session the user has already consented to.
///
/// Subscribed *before* `Start` on purpose. The portal reports the selection
/// already on the clipboard as a `SelectionOwnerChanged` when the session starts,
/// and a subscription taken out afterwards can miss it — which would leave
/// `available_formats` empty until the user next copied something, for no reason
/// the user could see.
pub async fn request<'a>(
    proxy: &'a Clipboard,
    session: &Session<RemoteDesktop>,
) -> Option<Portal<'a>> {
    let owner_changes = match proxy
        .receive_selection_owner_changed::<RemoteDesktop>()
        .await
    {
        Ok(stream) => Box::pin(stream) as OwnerChanges<'a>,
        Err(e) => {
            tracing::warn!(error = %e, "cannot watch the clipboard for changes; clipboard sync is unavailable");
            return None;
        }
    };
    let transfers = match proxy.receive_selection_transfer::<RemoteDesktop>().await {
        Ok(stream) => Box::pin(stream) as Transfers<'a>,
        Err(e) => {
            // Without this, an offer this machine makes could never be serviced
            // and every paste from it in another application would hang. Refusing
            // the clipboard outright is the honest answer.
            tracing::warn!(error = %e, "cannot answer clipboard paste requests; clipboard sync is unavailable");
            return None;
        }
    };

    if let Err(e) = proxy
        .request(session, RequestClipboardOptions::default())
        .await
    {
        tracing::warn!(error = %e, "the desktop portal would not add clipboard access to the session");
        return None;
    }

    Some(Portal {
        owner_changes,
        transfers,
    })
}

/// The synchronous side of a granted clipboard, for [`ClipboardState::attach`].
pub fn transport(commands: mpsc::UnboundedSender<Command>) -> Arc<dyn SelectionTransport> {
    Arc::new(Requests(commands))
}

/// One thing the synchronous trait side wants the portal thread to do.
///
/// The reply channel is a `std::sync::mpsc` rather than a tokio one because the
/// waiting end is an ordinary blocking thread with no runtime on it.
pub enum Command {
    Read {
        mime: String,
        reply: std::sync::mpsc::SyncSender<Result<Vec<u8>>>,
    },
    SetSelection {
        mimes: Vec<&'static str>,
        reply: std::sync::mpsc::SyncSender<Result<()>>,
    },
}

struct Requests(mpsc::UnboundedSender<Command>);

impl Requests {
    /// Post a command and wait for the portal thread's answer.
    fn ask<T>(
        &self,
        build: impl FnOnce(std::sync::mpsc::SyncSender<Result<T>>) -> Command,
    ) -> Result<T> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.0.send(build(tx)).map_err(|_| gone())?;
        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(answer) => answer,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(PlatformError::Other(format!(
                "the desktop portal did not answer a clipboard request within {}s",
                REQUEST_TIMEOUT.as_secs()
            ))),
            // The reply channel was dropped, which only happens when the session
            // has ended underneath the caller.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(gone()),
        }
    }
}

fn gone() -> PlatformError {
    PlatformError::Other("the desktop portal clipboard session has ended".into())
}

impl SelectionTransport for Requests {
    fn selection_read(&self, mime: &str) -> Result<Vec<u8>> {
        self.ask(|reply| Command::Read {
            mime: mime.to_string(),
            reply,
        })
    }

    fn set_selection(&self, mimes: &[&'static str]) -> Result<()> {
        self.ask(|reply| Command::SetSelection {
            mimes: mimes.to_vec(),
            reply,
        })
    }
}

/// Route one command onto a task of its own.
///
/// Spawned rather than awaited so that a transfer in flight cannot delay
/// revocation, shutdown, or the next injected event — the session's `select!`
/// loop is the only thing keeping any of those responsive.
pub fn dispatch(command: Command, proxy: &Arc<Clipboard>, session: &Arc<Session<RemoteDesktop>>) {
    let proxy = Arc::clone(proxy);
    let session = Arc::clone(session);
    match command {
        Command::Read { mime, reply } => {
            tokio::spawn(async move {
                let answer = match tokio::time::timeout(
                    TRANSFER_TIMEOUT,
                    read_selection(&proxy, &session, &mime),
                )
                .await
                {
                    Ok(answer) => answer,
                    Err(_) => Err(PlatformError::Other(format!(
                        "reading {mime} off the clipboard did not finish within {}s",
                        TRANSFER_TIMEOUT.as_secs()
                    ))),
                };
                // A caller that has already timed out is gone; nothing to report.
                let _ = reply.send(answer);
            });
        }
        Command::SetSelection { mimes, reply } => {
            tokio::spawn(async move {
                let borrowed: Vec<&str> = mimes.to_vec();
                let answer = proxy
                    .set_selection(
                        &session,
                        SetSelectionOptions::default().set_mime_types(&borrowed),
                    )
                    .await
                    .map_err(|e| {
                        PlatformError::Other(format!("offering a clipboard selection: {e}"))
                    });
                let _ = reply.send(answer);
            });
        }
    }
}

/// Answer one `SelectionTransfer`: somebody is pasting from our offer.
///
/// Also spawned, for the sharper version of the same reason: the application at
/// the other end is blocked in its own paste until these bytes arrive, and the
/// loop routing this signal is also the loop that would notice the session being
/// revoked.
pub fn serve_transfer(
    proxy: &Arc<Clipboard>,
    session: &Arc<Session<RemoteDesktop>>,
    state: &ClipboardState,
    mime: String,
    serial: u32,
) {
    let proxy = Arc::clone(proxy);
    let session = Arc::clone(session);
    // Looked up here rather than in the task, so the answer is the offer that was
    // standing when the request arrived.
    let payload = state.staged_for(&mime);
    tokio::spawn(async move {
        let served = match tokio::time::timeout(
            TRANSFER_TIMEOUT,
            write_selection(&proxy, &session, serial, payload.as_deref()),
        )
        .await
        {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                tracing::warn!(mime = %mime, error = %e, "could not answer a clipboard paste");
                false
            }
            Err(_) => {
                tracing::warn!(mime = %mime, "a clipboard paste was not drained in time");
                false
            }
        };
        // Always sent, success or not. The portal is holding a paste open on this
        // serial, and an application waiting for an answer that never comes is
        // precisely the failure this backend must not produce.
        if let Err(e) = proxy.selection_write_done(&session, serial, served).await {
            tracing::warn!(error = %e, "could not tell the portal a clipboard transfer had finished");
        }
    });
}

/// Drain a `SelectionRead` pipe, refusing anything the protocol cannot carry.
async fn read_selection(
    proxy: &Clipboard,
    session: &Session<RemoteDesktop>,
    mime: &str,
) -> Result<Vec<u8>> {
    let fd = proxy
        .selection_read(session, mime)
        .await
        .map_err(|e| PlatformError::Other(format!("reading {mime} off the clipboard: {e}")))?;

    let mut pipe = tokio::net::unix::pipe::Receiver::from_owned_fd(std::os::fd::OwnedFd::from(fd))
        .map_err(|e| PlatformError::Other(format!("opening the clipboard pipe: {e}")))?;

    let mut out = Vec::new();
    let mut chunk = vec![0u8; CHUNK];
    loop {
        let read = pipe
            .read(&mut chunk)
            .await
            .map_err(|e| PlatformError::Other(format!("draining the clipboard pipe: {e}")))?;
        if read == 0 {
            break;
        }
        // Checked as it arrives rather than at the end. The point of the limit is
        // not to hold a payload the protocol cannot carry, so reading all of a
        // 2 GiB image before saying so would defeat it.
        if out.len() + read > MAX_CLIPBOARD_BYTES {
            return Err(PlatformError::Other(format!(
                "the clipboard holds more than {MAX_CLIPBOARD_BYTES} bytes of {mime}; \
                 the protocol cannot carry it"
            )));
        }
        out.extend_from_slice(&chunk[..read]);
    }
    Ok(out)
}

/// Fill a `SelectionWrite` pipe with the bytes behind our offer.
///
/// A transfer asking for a MIME type this backend no longer has bytes for still
/// takes the descriptor and closes it: the pasting application then sees an empty
/// selection, which is a paste that produced nothing rather than one that never
/// returned.
async fn write_selection(
    proxy: &Clipboard,
    session: &Session<RemoteDesktop>,
    serial: u32,
    payload: Option<&[u8]>,
) -> Result<()> {
    let fd = proxy
        .selection_write(session, serial)
        .await
        .map_err(|e| PlatformError::Other(format!("answering a clipboard paste: {e}")))?;

    let mut pipe = tokio::net::unix::pipe::Sender::from_owned_fd(std::os::fd::OwnedFd::from(fd))
        .map_err(|e| PlatformError::Other(format!("opening the clipboard pipe: {e}")))?;

    let result = match payload {
        Some(bytes) => pipe
            .write_all(bytes)
            .await
            .map_err(|e| PlatformError::Other(format!("writing the clipboard pipe: {e}"))),
        None => Err(PlatformError::ClipboardEmpty),
    };
    // Closed before the completion is reported, so the reader sees EOF rather
    // than waiting on a descriptor this process still holds.
    drop(pipe);
    result
}
