//! Text clipboard over the CLIPRDR static channel (MS-RDPECLIP).
//!
//! IronRDP calls the backend synchronously while it processes channel data; the backend only
//! forwards those calls as [`Signal`]s, which the session loop handles with access to the channel.
//! Remote text is fetched as soon as the remote copies it; local text is announced when the UI
//! reports it and sent when the remote pastes.

use ironrdp::cliprdr::CliprdrClient;
use ironrdp::cliprdr::CliprdrSvcMessages;
use ironrdp::cliprdr::backend::CliprdrBackend;
use ironrdp::cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest, FileContentsResponse,
    FormatDataRequest, FormatDataResponse, LockDataId, OwnedFormatDataResponse,
};
use ironrdp::pdu::PduResult;
use ironrdp_cliprdr_format::bitmap::{dib_to_png, dibv5_to_png, png_to_cf_dib, png_to_cf_dibv5};
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

/// Larger clipboard text is ignored rather than copied (protects both sides from huge transfers).
pub(crate) const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub(crate) enum Signal {
    /// Channel initialisation: announce what the local clipboard has.
    RequestFormatList,
    /// The channel is fully set up.
    Ready,
    /// The remote copied something.
    RemoteCopy(Vec<ClipboardFormatId>),
    /// The remote pastes and wants our data.
    DataRequest(ClipboardFormatId),
    /// Data we asked for; `None` if the remote couldn't provide it.
    DataResponse(Option<Vec<u8>>),
}

#[derive(Debug)]
pub(crate) struct Backend {
    signals: mpsc::UnboundedSender<Signal>,
}

impl Backend {
    pub(crate) fn new() -> (Self, mpsc::UnboundedReceiver<Signal>) {
        let (signals, receiver) = mpsc::unbounded_channel();
        (Self { signals }, receiver)
    }

    fn signal(&self, signal: Signal) {
        let _ = self.signals.send(signal);
    }
}

ironrdp::core::impl_as_any!(Backend);

impl CliprdrBackend for Backend {
    fn temporary_directory(&self) -> &str {
        // Only used for file transfers, which we don't offer.
        ".cliprdr"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {
        self.signal(Signal::Ready);
    }

    fn on_request_format_list(&mut self) {
        self.signal(Signal::RequestFormatList);
    }

    fn on_process_negotiated_capabilities(&mut self, _capabilities: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        self.signal(Signal::RemoteCopy(available_formats.iter().map(|f| f.id).collect()));
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        self.signal(Signal::DataRequest(request.format));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let data = (!response.is_error()).then(|| response.data().to_vec());
        self.signal(Signal::DataResponse(data));
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

/// What the local clipboard holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClipboardContent {
    pub text: Option<String>,
    /// An image, PNG-encoded.
    pub png: Option<Vec<u8>>,
}

impl ClipboardContent {
    fn is_empty(&self) -> bool {
        self.text.is_none() && self.png.is_none()
    }
}

/// What a remote copy delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteClipboard {
    Text(String),
    /// PNG-encoded.
    Image(Vec<u8>),
}

/// Clipboard state for one session.
#[derive(Default)]
pub(crate) struct Clipboard {
    /// The local clipboard, as reported by the UI.
    local: ClipboardContent,
    /// What we last told the remote we have.
    announced: ClipboardContent,
    /// The channel finished initialising; copies can be announced directly.
    ready: bool,
    /// The format we asked the remote for.
    awaiting: Option<ClipboardFormatId>,
}

/// What handling a signal produced.
#[derive(Default)]
pub(crate) struct Outcome {
    pub messages: Option<CliprdrSvcMessages<ironrdp::cliprdr::Client>>,
    /// What the remote copied, for the local clipboard.
    pub remote: Option<RemoteClipboard>,
}

impl Clipboard {
    pub(crate) fn handle(&mut self, signal: Signal, channel: &mut CliprdrClient) -> PduResult<Outcome> {
        let mut outcome = Outcome::default();
        match signal {
            Signal::RequestFormatList => outcome.messages = Some(self.announce(channel)?),
            Signal::Ready => {
                self.ready = true;
                if self.announced != self.local {
                    outcome.messages = Some(self.announce(channel)?);
                }
            }
            Signal::RemoteCopy(formats) => {
                // The remote owns the clipboard now; our earlier announcement is void.
                self.announced = ClipboardContent::default();
                // Text first; otherwise an image (DIBV5 keeps transparency).
                let wanted = [
                    ClipboardFormatId::CF_UNICODETEXT,
                    ClipboardFormatId::CF_DIBV5,
                    ClipboardFormatId::CF_DIB,
                ]
                .into_iter()
                .find(|f| formats.contains(f));
                match wanted {
                    Some(format) => {
                        self.awaiting = Some(format);
                        outcome.messages = Some(channel.initiate_paste(format)?);
                    }
                    None => debug!(?formats, "Remote clipboard has nothing we can use"),
                }
            }
            Signal::DataRequest(format) => {
                let response = self.local_data(format).unwrap_or_else(|| {
                    debug!(?format, "Remote asked for a format we don't have");
                    OwnedFormatDataResponse::new_error()
                });
                outcome.messages = Some(channel.submit_format_data(response)?);
            }
            Signal::DataResponse(data) => {
                let Some(format) = self.awaiting.take() else {
                    return Ok(outcome);
                };
                let Some(data) = data else {
                    debug!("Remote clipboard data unavailable");
                    return Ok(outcome);
                };
                if data.len() > MAX_TEXT_BYTES * 4 {
                    warn!(bytes = data.len(), "Remote clipboard data too large; ignored");
                    return Ok(outcome);
                }
                outcome.remote = match format {
                    ClipboardFormatId::CF_UNICODETEXT if data.len() <= MAX_TEXT_BYTES => {
                        Some(RemoteClipboard::Text(from_windows_text(&data)))
                    }
                    ClipboardFormatId::CF_DIBV5 => image(dibv5_to_png(&data)),
                    ClipboardFormatId::CF_DIB => image(dib_to_png(&data)),
                    _ => None,
                };
            }
        }
        Ok(outcome)
    }

    fn local_data(&self, format: ClipboardFormatId) -> Option<OwnedFormatDataResponse> {
        match format {
            ClipboardFormatId::CF_UNICODETEXT => {
                let text = self.local.text.as_ref()?;
                Some(OwnedFormatDataResponse::new_unicode_string(&to_windows_text(text)))
            }
            ClipboardFormatId::CF_DIB | ClipboardFormatId::CF_DIBV5 => {
                let png = self.local.png.as_ref()?;
                let dib = if format == ClipboardFormatId::CF_DIBV5 {
                    png_to_cf_dibv5(png)
                } else {
                    png_to_cf_dib(png)
                };
                match dib {
                    Ok(dib) => Some(OwnedFormatDataResponse::new_data(dib)),
                    Err(e) => {
                        warn!(error = %e, "Couldn't convert the clipboard image");
                        None
                    }
                }
            }
            _ => None,
        }
    }

    /// The local clipboard changed.
    pub(crate) fn set_local(
        &mut self,
        mut content: ClipboardContent,
        channel: &mut CliprdrClient,
    ) -> PduResult<Option<CliprdrSvcMessages<ironrdp::cliprdr::Client>>> {
        if content.text.as_ref().is_some_and(|t| t.len() > MAX_TEXT_BYTES) {
            warn!("Local clipboard text too large; not shared");
            content.text = None;
        }
        if content.png.as_ref().is_some_and(|p| p.len() > MAX_TEXT_BYTES * 4) {
            warn!("Local clipboard image too large; not shared");
            content.png = None;
        }
        self.local = content;
        // Before the channel is ready the content goes out with the initial format list instead.
        if !self.ready || self.announced == self.local {
            return Ok(None);
        }
        self.announce(channel).map(Some)
    }

    fn announce(&mut self, channel: &mut CliprdrClient) -> PduResult<CliprdrSvcMessages<ironrdp::cliprdr::Client>> {
        let mut formats = Vec::new();
        if self.local.text.is_some() {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT));
        }
        if self.local.png.is_some() {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_DIBV5));
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_DIB));
        }
        if self.local.is_empty() {
            trace!("Local clipboard is empty");
        }
        debug!(?formats, "Offering the local clipboard");
        self.announced = self.local.clone();
        channel.initiate_copy(&formats)
    }
}

fn image(png: Result<Vec<u8>, ironrdp_cliprdr_format::bitmap::BitmapError>) -> Option<RemoteClipboard> {
    match png {
        Ok(png) => Some(RemoteClipboard::Image(png)),
        Err(e) => {
            warn!(error = %e, "Couldn't convert the remote clipboard image");
            None
        }
    }
}

/// UTF-16LE, NUL-terminated, CRLF line endings → a Rust string with LF line endings.
fn from_windows_text(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .take_while(|&unit| unit != 0)
        .collect();
    String::from_utf16_lossy(&units).replace("\r\n", "\n")
}

/// LF line endings → CRLF, as Windows applications expect.
fn to_windows_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn converts_line_endings_and_terminator() {
        assert_eq!(from_windows_text(&utf16("a\r\nb – ü\r\n")), "a\nb – ü\n");
        // Anything after the terminator is padding.
        let mut padded = utf16("x");
        padded.extend([0x41, 0, 0, 0]);
        assert_eq!(from_windows_text(&padded), "x");
        assert_eq!(to_windows_text("a\nb\r\nc"), "a\r\nb\r\nc");
    }
}
