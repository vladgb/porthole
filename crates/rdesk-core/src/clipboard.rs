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
use tokio::sync::mpsc;
use tracing::{debug, warn};

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

/// Clipboard state for one session.
#[derive(Default)]
pub(crate) struct Clipboard {
    /// The local clipboard's text, as reported by the UI.
    local: Option<String>,
    /// What we last told the remote we have (`None`: nothing).
    announced: Option<String>,
    /// The channel finished initialising; copies can be announced directly.
    ready: bool,
    /// We asked the remote for its text.
    awaiting_text: bool,
}

/// What handling a signal produced.
#[derive(Default)]
pub(crate) struct Outcome {
    pub messages: Option<CliprdrSvcMessages<ironrdp::cliprdr::Client>>,
    /// Text the remote copied, for the local clipboard.
    pub remote_text: Option<String>,
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
                self.announced = None;
                if formats.contains(&ClipboardFormatId::CF_UNICODETEXT) {
                    self.awaiting_text = true;
                    outcome.messages = Some(channel.initiate_paste(ClipboardFormatId::CF_UNICODETEXT)?);
                } else {
                    debug!(?formats, "Remote clipboard has no text");
                }
            }
            Signal::DataRequest(format) => {
                let response = match (&self.local, format) {
                    (Some(text), ClipboardFormatId::CF_UNICODETEXT) => {
                        OwnedFormatDataResponse::new_unicode_string(&to_windows_text(text))
                    }
                    _ => {
                        debug!(?format, "Remote asked for a format we don't have");
                        OwnedFormatDataResponse::new_error()
                    }
                };
                outcome.messages = Some(channel.submit_format_data(response)?);
            }
            Signal::DataResponse(data) => {
                if !std::mem::take(&mut self.awaiting_text) {
                    return Ok(outcome);
                }
                match data {
                    Some(data) if data.len() > MAX_TEXT_BYTES => {
                        warn!(bytes = data.len(), "Remote clipboard text too large; ignored");
                    }
                    Some(data) => outcome.remote_text = Some(from_windows_text(&data)),
                    None => debug!("Remote clipboard data unavailable"),
                }
            }
        }
        Ok(outcome)
    }

    /// The local clipboard changed (`None`: it holds no text).
    pub(crate) fn set_local(
        &mut self,
        text: Option<String>,
        channel: &mut CliprdrClient,
    ) -> PduResult<Option<CliprdrSvcMessages<ironrdp::cliprdr::Client>>> {
        if text.as_ref().is_some_and(|t| t.len() > MAX_TEXT_BYTES) {
            warn!("Local clipboard text too large; not shared");
            self.local = None;
        } else {
            self.local = text;
        }
        // Before the channel is ready the text goes out with the initial format list instead.
        if !self.ready || self.announced == self.local {
            return Ok(None);
        }
        self.announce(channel).map(Some)
    }

    fn announce(&mut self, channel: &mut CliprdrClient) -> PduResult<CliprdrSvcMessages<ironrdp::cliprdr::Client>> {
        let formats: &[ClipboardFormat] = if self.local.is_some() {
            &[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]
        } else {
            &[]
        };
        self.announced = self.local.clone();
        channel.initiate_copy(formats)
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
