use std::{
    fmt,
    io::{self, Read, Write},
};

pub const SAMPLE_RATE_HZ: u32 = 16_000;
pub const MAX_SAMPLE_COUNT: usize = SAMPLE_RATE_HZ as usize * 60;
pub const MAX_TRANSCRIPT_BYTES: usize = 64 * 1024;

const REQUEST_MAGIC: [u8; 4] = *b"AJAR";
const RESPONSE_MAGIC: [u8; 4] = *b"AJAS";
const PROTOCOL_VERSION: u16 = 1;

#[derive(Clone, Debug, PartialEq)]
pub struct WorkerRequest {
    pub request_id: u64,
    pub sample_rate_hz: u32,
    pub samples: Vec<f32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum WorkerStatus {
    Success = 0,
    InvalidAudio = 1,
    InferenceFailed = 2,
}

impl WorkerStatus {
    fn from_wire(value: u16) -> Result<Self, ProtocolError> {
        match value {
            0 => Ok(Self::Success),
            1 => Ok(Self::InvalidAudio),
            2 => Ok(Self::InferenceFailed),
            _ => Err(ProtocolError::InvalidFrame),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerResponse {
    pub request_id: u64,
    pub status: WorkerStatus,
    pub transcript: String,
}

#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    InvalidFrame,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Io(_) => "ASR worker pipe I/O failed",
            Self::InvalidFrame => "ASR worker returned an invalid frame",
        })
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::InvalidFrame => None,
        }
    }
}

impl From<io::Error> for ProtocolError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub fn write_request(
    writer: &mut (impl Write + ?Sized),
    request: &WorkerRequest,
) -> Result<(), ProtocolError> {
    validate_samples(request.sample_rate_hz, &request.samples)?;
    writer.write_all(&REQUEST_MAGIC)?;
    write_u16(writer, PROTOCOL_VERSION)?;
    write_u16(writer, 0)?;
    write_u64(writer, request.request_id)?;
    write_u32(writer, request.sample_rate_hz)?;
    write_u32(writer, request.samples.len() as u32)?;
    for sample in &request.samples {
        writer.write_all(&sample.to_bits().to_le_bytes())?;
    }
    writer.flush()?;
    Ok(())
}

pub fn read_request(
    reader: &mut (impl Read + ?Sized),
) -> Result<Option<WorkerRequest>, ProtocolError> {
    let Some(magic) = read_magic_or_eof(reader)? else {
        return Ok(None);
    };
    if magic != REQUEST_MAGIC || read_u16(reader)? != PROTOCOL_VERSION || read_u16(reader)? != 0 {
        return Err(ProtocolError::InvalidFrame);
    }
    let request_id = read_u64(reader)?;
    let sample_rate_hz = read_u32(reader)?;
    let sample_count = read_u32(reader)? as usize;
    if sample_rate_hz != SAMPLE_RATE_HZ || sample_count == 0 || sample_count > MAX_SAMPLE_COUNT {
        return Err(ProtocolError::InvalidFrame);
    }
    let mut samples = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        let mut bytes = [0; 4];
        reader.read_exact(&mut bytes)?;
        let sample = f32::from_bits(u32::from_le_bytes(bytes));
        if !sample.is_finite() || !(-1.0..=1.0).contains(&sample) {
            return Err(ProtocolError::InvalidFrame);
        }
        samples.push(sample);
    }
    Ok(Some(WorkerRequest {
        request_id,
        sample_rate_hz,
        samples,
    }))
}

pub fn write_response(
    writer: &mut (impl Write + ?Sized),
    response: &WorkerResponse,
) -> Result<(), ProtocolError> {
    let transcript = response.transcript.as_bytes();
    if transcript.len() > MAX_TRANSCRIPT_BYTES
        || (response.status != WorkerStatus::Success && !transcript.is_empty())
    {
        return Err(ProtocolError::InvalidFrame);
    }
    writer.write_all(&RESPONSE_MAGIC)?;
    write_u16(writer, PROTOCOL_VERSION)?;
    write_u16(writer, response.status as u16)?;
    write_u64(writer, response.request_id)?;
    write_u32(writer, transcript.len() as u32)?;
    writer.write_all(transcript)?;
    writer.flush()?;
    Ok(())
}

pub fn read_response(reader: &mut (impl Read + ?Sized)) -> Result<WorkerResponse, ProtocolError> {
    let magic = read_magic_or_eof(reader)?.ok_or_else(|| {
        ProtocolError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "ASR worker closed its response pipe",
        ))
    })?;
    if magic != RESPONSE_MAGIC || read_u16(reader)? != PROTOCOL_VERSION {
        return Err(ProtocolError::InvalidFrame);
    }
    let status = WorkerStatus::from_wire(read_u16(reader)?)?;
    let request_id = read_u64(reader)?;
    let length = read_u32(reader)? as usize;
    if length > MAX_TRANSCRIPT_BYTES || (status != WorkerStatus::Success && length != 0) {
        return Err(ProtocolError::InvalidFrame);
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    let transcript = String::from_utf8(bytes).map_err(|_| ProtocolError::InvalidFrame)?;
    Ok(WorkerResponse {
        request_id,
        status,
        transcript,
    })
}

fn validate_samples(sample_rate_hz: u32, samples: &[f32]) -> Result<(), ProtocolError> {
    if sample_rate_hz != SAMPLE_RATE_HZ
        || samples.is_empty()
        || samples.len() > MAX_SAMPLE_COUNT
        || samples
            .iter()
            .any(|sample| !sample.is_finite() || !(-1.0..=1.0).contains(sample))
    {
        return Err(ProtocolError::InvalidFrame);
    }
    Ok(())
}

fn read_magic_or_eof(reader: &mut (impl Read + ?Sized)) -> Result<Option<[u8; 4]>, ProtocolError> {
    let mut magic = [0; 4];
    match reader.read(&mut magic[..1])? {
        0 => return Ok(None),
        1 => {}
        _ => unreachable!("one-byte read returned more than one byte"),
    }
    reader.read_exact(&mut magic[1..])?;
    Ok(Some(magic))
}

fn write_u16(writer: &mut (impl Write + ?Sized), value: u16) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_u32(writer: &mut (impl Write + ?Sized), value: u32) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_u64(writer: &mut (impl Write + ?Sized), value: u64) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn read_u16(reader: &mut (impl Read + ?Sized)) -> io::Result<u16> {
    let mut bytes = [0; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32(reader: &mut (impl Read + ?Sized)) -> io::Result<u32> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut (impl Read + ?Sized)) -> io::Result<u64> {
    let mut bytes = [0; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip_preserves_pcm_without_text_encoding() {
        let request = WorkerRequest {
            request_id: 42,
            sample_rate_hz: SAMPLE_RATE_HZ,
            samples: vec![-1.0, -0.25, 0.0, 0.5, 1.0],
        };
        let mut wire = Vec::new();
        write_request(&mut wire, &request).expect("write request");
        assert_eq!(wire.len(), 24 + request.samples.len() * 4);
        assert_eq!(
            read_request(&mut wire.as_slice()).expect("read request"),
            Some(request)
        );
    }

    #[test]
    fn response_round_trip_preserves_unicode_transcript() {
        let response = WorkerResponse {
            request_id: 7,
            status: WorkerStatus::Success,
            transcript: "你好, Jarvis".into(),
        };
        let mut wire = Vec::new();
        write_response(&mut wire, &response).expect("write response");
        assert_eq!(
            read_response(&mut wire.as_slice()).expect("read response"),
            response
        );
    }

    #[test]
    fn protocol_rejects_unbounded_or_non_finite_audio_before_allocation() {
        let mut oversized = Vec::new();
        oversized.extend_from_slice(&REQUEST_MAGIC);
        oversized.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        oversized.extend_from_slice(&0_u16.to_le_bytes());
        oversized.extend_from_slice(&1_u64.to_le_bytes());
        oversized.extend_from_slice(&SAMPLE_RATE_HZ.to_le_bytes());
        oversized.extend_from_slice(&((MAX_SAMPLE_COUNT + 1) as u32).to_le_bytes());
        assert!(matches!(
            read_request(&mut oversized.as_slice()),
            Err(ProtocolError::InvalidFrame)
        ));

        for sample in [f32::NAN, f32::INFINITY, 1.01] {
            assert!(write_request(
                &mut Vec::new(),
                &WorkerRequest {
                    request_id: 1,
                    sample_rate_hz: SAMPLE_RATE_HZ,
                    samples: vec![sample],
                },
            )
            .is_err());
        }
    }

    #[test]
    fn protocol_rejects_truncated_and_unknown_frames() {
        assert!(read_request(&mut b"AJA".as_slice()).is_err());
        assert!(read_response(&mut b"wrong frame".as_slice()).is_err());
        assert_eq!(read_request(&mut [].as_slice()).expect("clean eof"), None);
    }
}
