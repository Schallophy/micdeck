use std::io::{self, Seek, Write};

/// Streaming 44-byte-header WAV writer. `finish` patches the two size fields and is idempotent, and
/// `Drop` calls it, so an early return or a panic at the end of a long run still leaves a readable
/// file instead of a header that claims zero samples.
pub struct WavWriter {
    file: Option<std::fs::File>,
    data_bytes: u64,
    sample_rate: u32,
    channels: u16,
    bits: u16,
    float: bool,
}

impl WavWriter {
    pub fn create(path: &str, sample_rate: u32, channels: u16, float: bool) -> io::Result<Self> {
        let file = std::fs::File::create(path)?;
        let mut writer = Self {
            file: Some(file),
            data_bytes: 0,
            sample_rate,
            channels,
            bits: if float { 32 } else { 16 },
            float,
        };
        writer.write_header(0)?;
        Ok(writer)
    }

    fn format_tag(&self) -> u16 {
        if self.float {
            0xFFFE // WAVE_FORMAT_EXTENSIBLE, as WASAPI reports
        } else {
            1 // PCM
        }
    }

    fn write_header(&mut self, data_bytes: u32) -> io::Result<()> {
        // Read every field out before borrowing the file, so the header build does not need `self`.
        let (sample_rate, channels, bits, float) =
            (self.sample_rate, self.channels, self.bits, self.float);
        let tag = self.format_tag();
        let block_align = channels * bits / 8;
        let byte_rate = sample_rate * block_align as u32;
        let file = self.file.as_mut().ok_or_else(empty)?;
        let mut head = Vec::with_capacity(44);
        let mut push = |bytes: &[u8]| head.extend_from_slice(bytes);
        fn u32le(v: u32) -> [u8; 4] {
            v.to_le_bytes()
        }
        fn u16le(v: u16) -> [u8; 2] {
            v.to_le_bytes()
        }
        push(b"RIFF");
        push(&u32le(36 + data_bytes));
        push(b"WAVE");
        push(b"fmt ");
        push(&u32le(if float { 40 } else { 16 }));
        push(&u16le(tag));
        push(&u16le(channels));
        push(&u32le(sample_rate));
        push(&u32le(byte_rate));
        push(&u16le(block_align));
        push(&u16le(bits));
        if float {
            // Extensible header: 22 bytes of extra fields, cbSize must be 22.
            push(&u16le(22));
            push(&u16le(bits));
            let mask = (1u32 << channels as u32) - 1;
            push(&u32le(mask));
            // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT = {00000003-0000-0010-8000-00aa00389b71}
            push(&[0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00]);
            push(&[0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71]);
        }
        push(b"data");
        push(&u32le(data_bytes));
        file.rewind()?;
        file.write_all(&head)?;
        Ok(())
    }

    pub fn write_f32(&mut self, samples: &[f32]) -> io::Result<()> {
        let file = self.file.as_mut().ok_or_else(empty)?;
        if self.float {
            let mut buf = Vec::with_capacity(samples.len() * 4);
            for s in samples {
                buf.extend_from_slice(&s.to_le_bytes());
            }
            file.seek(std::io::SeekFrom::End(0))?;
            file.write_all(&buf)?;
            self.data_bytes += buf.len() as u64;
        } else {
            let mut buf = Vec::with_capacity(samples.len() * 2);
            for s in samples {
                let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                buf.extend_from_slice(&v.to_le_bytes());
            }
            file.seek(std::io::SeekFrom::End(0))?;
            file.write_all(&buf)?;
            self.data_bytes += buf.len() as u64;
        }
        Ok(())
    }

    pub fn seconds(&self) -> f64 {
        let bytes_per_frame = self.channels as u64 * self.bits as u64 / 8;
        self.data_bytes as f64 / (self.sample_rate as f64 * bytes_per_frame as f64)
    }

    pub fn finish(&mut self) -> io::Result<()> {
        if self.file.is_none() {
            return Ok(());
        }
        self.write_header(self.data_bytes.min(u32::MAX as u64) as u32)?;
        if let Some(file) = self.file.as_mut() {
            file.flush()?;
        }
        self.file = None;
        Ok(())
    }
}

impl Drop for WavWriter {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn empty() -> io::Error {
    io::Error::new(io::ErrorKind::WriteZero, "wav writer already finished")
}
