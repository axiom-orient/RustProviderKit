use rust_provider_kit_core::{ProviderFailure, ProviderFailureCode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerSentEvent {
    pub(crate) event: Option<String>,
    pub(crate) data: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ServerSentEventDecoder {
    maximum_line_bytes: usize,
    maximum_event_bytes: usize,
    line: Vec<u8>,
    pending_cr: bool,
    prefix: Vec<u8>,
    bom_checked: bool,
    event_name: Option<String>,
    data: String,
    has_data: bool,
    scanned_byte_count: usize,
}
impl ServerSentEventDecoder {
    #[cfg(test)]
    pub(crate) fn new(
        maximum_line_bytes: usize,
        maximum_event_bytes: usize,
    ) -> Result<Self, ProviderFailure> {
        if maximum_line_bytes == 0
            || maximum_event_bytes == 0
            || maximum_line_bytes > maximum_event_bytes
        {
            return Err(malformed("SSE bounds are invalid"));
        }
        Ok(Self {
            maximum_line_bytes,
            maximum_event_bytes,
            line: Vec::new(),
            pending_cr: false,
            prefix: Vec::new(),
            bom_checked: false,
            event_name: None,
            data: String::new(),
            has_data: false,
            scanned_byte_count: 0,
        })
    }
    #[must_use]
    pub(crate) fn defaults() -> Self {
        Self {
            maximum_line_bytes: 1_048_576,
            maximum_event_bytes: 4 * 1_024 * 1_024,
            line: Vec::new(),
            pending_cr: false,
            prefix: Vec::new(),
            bom_checked: false,
            event_name: None,
            data: String::new(),
            has_data: false,
            scanned_byte_count: 0,
        }
    }
    #[cfg(test)]
    #[must_use]
    pub(crate) fn scanned_byte_count(&self) -> usize {
        self.scanned_byte_count
    }
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<ServerSentEvent>, ProviderFailure> {
        let mut output = Vec::new();
        for &byte in bytes {
            self.scanned_byte_count = self
                .scanned_byte_count
                .checked_add(1)
                .ok_or_else(|| response_too_large("SSE scan count overflow"))?;
            if !self.bom_checked {
                self.prefix.push(byte);
                if self.prefix.len() < 3 {
                    continue;
                }
                self.bom_checked = true;
                let prefix = std::mem::take(&mut self.prefix);
                let slice = if prefix == [0xEF, 0xBB, 0xBF] {
                    &prefix[3..]
                } else {
                    &prefix[..]
                };
                for &value in slice {
                    self.consume_byte(value, &mut output)?;
                }
                continue;
            }
            self.consume_byte(byte, &mut output)?;
        }
        Ok(output)
    }
    pub(crate) fn finish(&mut self) -> Result<Vec<ServerSentEvent>, ProviderFailure> {
        let mut output = Vec::new();
        if !self.bom_checked {
            self.bom_checked = true;
            let prefix = std::mem::take(&mut self.prefix);
            let slice = if prefix == [0xEF, 0xBB, 0xBF] {
                &prefix[3..]
            } else {
                &prefix[..]
            };
            for &value in slice {
                self.consume_byte(value, &mut output)?;
            }
        }
        if !self.line.is_empty() {
            self.process_line(&mut output)?;
        }
        if let Some(event) = self.dispatch() {
            output.push(event);
        }
        Ok(output)
    }
    fn consume_byte(
        &mut self,
        byte: u8,
        output: &mut Vec<ServerSentEvent>,
    ) -> Result<(), ProviderFailure> {
        if self.pending_cr {
            self.pending_cr = false;
            if byte == b'\n' {
                return Ok(());
            }
        }
        match byte {
            b'\r' => {
                self.process_line(output)?;
                self.pending_cr = true;
            }
            b'\n' => {
                self.process_line(output)?;
            }
            _ => {
                let next = self
                    .line
                    .len()
                    .checked_add(1)
                    .ok_or_else(|| response_too_large("SSE line exceeded its bound"))?;
                if next > self.maximum_line_bytes {
                    return Err(response_too_large("SSE line exceeded its bound"));
                }
                self.line.push(byte);
            }
        }
        Ok(())
    }
    fn process_line(&mut self, output: &mut Vec<ServerSentEvent>) -> Result<(), ProviderFailure> {
        let bytes = std::mem::take(&mut self.line);
        let line = std::str::from_utf8(&bytes).map_err(|_| malformed("SSE line is not UTF-8"))?;
        if line.is_empty() {
            if let Some(event) = self.dispatch() {
                output.push(event);
            }
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, mut value) = match line.split_once(':') {
            Some((field, value)) => (field, value),
            None => (line, ""),
        };
        if let Some(stripped) = value.strip_prefix(' ') {
            value = stripped;
        }
        match field {
            "event" => self.event_name = Some(value.to_owned()),
            "data" => {
                let separator = usize::from(self.has_data);
                let next = self
                    .data
                    .len()
                    .checked_add(separator)
                    .and_then(|count| count.checked_add(value.len()))
                    .ok_or_else(|| response_too_large("SSE event data exceeded its bound"))?;
                if next > self.maximum_event_bytes {
                    return Err(response_too_large("SSE event data exceeded its bound"));
                }
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            _ => {}
        }
        Ok(())
    }
    fn dispatch(&mut self) -> Option<ServerSentEvent> {
        if !self.has_data {
            self.event_name = None;
            return None;
        }
        let data = std::mem::take(&mut self.data);
        self.has_data = false;
        Some(ServerSentEvent {
            event: self.event_name.take(),
            data,
        })
    }
}
fn malformed(message: &str) -> ProviderFailure {
    ProviderFailure::new(ProviderFailureCode::MalformedResponse, message)
}
fn response_too_large(message: &str) -> ProviderFailure {
    ProviderFailure::new(ProviderFailureCode::ResponseTooLarge, message)
}
