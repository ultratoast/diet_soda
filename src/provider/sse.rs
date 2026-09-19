//! Incremental SSE framing, shared by providers and MCP. Only complete UTF-8
//! lines are decoded; consuming a chunk shifts the remaining buffer once.
use anyhow::{bail, Result};

#[derive(Default)]
pub struct SseDecoder {
    pending: Vec<u8>,
    data: Vec<String>,
    data_bytes: usize,
}
impl SseDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        self.pending.extend_from_slice(bytes);
        let mut events = vec![];
        let mut start = 0;
        while let Some(end) = self.pending[start..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|n| start + n)
        {
            let line = std::str::from_utf8(&self.pending[start..end])?.trim_end_matches('\r');
            if line.is_empty() && !self.data.is_empty() {
                events.push(self.data.join("\n"));
                self.data.clear();
                self.data_bytes = 0;
            } else if let Some(data) = line.strip_prefix("data:") {
                let data = data.strip_prefix(' ').unwrap_or(data);
                self.data_bytes += data.len();
                if self.data_bytes > 2_000_000 {
                    bail!("SSE event exceeds size limit");
                }
                self.data.push(data.into());
            }
            start = end + 1;
        }
        self.pending.drain(..start);
        if self.pending.len() > 2_000_000 {
            bail!("SSE line exceeds size limit");
        }
        Ok(events)
    }
}
