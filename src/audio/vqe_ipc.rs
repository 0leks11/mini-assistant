use anyhow::{Context, Result};
use std::collections::VecDeque;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

const YIO_AUDIO_ADDR: &str = "127.0.0.1:9850";
const MAX_FRAME: usize = 65536;

/// Encodes an unsigned integer as varint
fn varint(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while n > 127 {
        out.push(((n & 127) | 128) as u8);
        n >>= 7;
    }
    out.push(n as u8);
    out
}

/// Encodes length-delimited wire type (wire type 2)
fn blob(field_num: u64, value: &[u8]) -> Vec<u8> {
    let tag = (field_num << 3) | 2;
    let mut out = varint(tag);
    out.extend_from_slice(&varint(value.len() as u64));
    out.extend_from_slice(value);
    out
}

/// Encodes varint wire type (wire type 0)
fn number(field_num: u64, value: u64) -> Vec<u8> {
    let tag = field_num << 3;
    let mut out = varint(tag);
    out.extend_from_slice(&varint(value));
    out
}

/// Builds IPC command request packet (member 159):
/// field 3 = request_vqe, field 6 = unsubscribe
fn build_ipc_request(field: u64) -> Vec<u8> {
    let body = blob(field, &[]);
    let mut payload = blob(252, &body);
    payload.extend_from_slice(&number(253, 159));

    let len_prefix = (payload.len() as u32).to_le_bytes();
    let mut packet = Vec::with_capacity(4 + payload.len());
    packet.extend_from_slice(&len_prefix);
    packet.extend_from_slice(&payload);
    packet
}

/// Parses Protobuf varint from slice
fn read_varint(buf: &[u8], offset: &mut usize) -> Option<u64> {
    let mut result = 0u64;
    let mut shift = 0;
    while *offset < buf.len() {
        let byte = buf[*offset];
        *offset += 1;
        result |= ((byte & 0x7F) as u64) << shift;
        if (byte & 0x80) == 0 {
            return Some(result);
        }
        shift += 7;
        if shift > 64 {
            return None;
        }
    }
    None
}

/// Extract vqe_0 PCM bytes from incoming payload
fn extract_vqe_pcm(payload: &[u8]) -> Option<Vec<u8>> {
    let mut offset = 0;
    let mut kind: Option<u64> = None;
    let mut body_slice: Option<&[u8]> = None;

    while offset < payload.len() {
        let tag = read_varint(payload, &mut offset)?;
        let field_num = tag >> 3;
        let wire_type = tag & 7;

        match wire_type {
            0 => {
                let val = read_varint(payload, &mut offset)?;
                if field_num == 253 {
                    kind = Some(val);
                }
            }
            2 => {
                let len = read_varint(payload, &mut offset)? as usize;
                if offset + len > payload.len() {
                    return None;
                }
                let bytes = &payload[offset..offset + len];
                offset += len;
                if field_num == 252 {
                    body_slice = Some(bytes);
                }
            }
            _ => return None,
        }
    }

    // Kind 158 is IOAudioChannelsData
    if kind != Some(158) {
        return None;
    }

    let body = body_slice?;
    let mut b_offset = 0;

    while b_offset < body.len() {
        let tag = read_varint(body, &mut b_offset)?;
        let field_num = tag >> 3;
        let wire_type = tag & 7;

        if wire_type != 2 {
            continue;
        }
        let len = read_varint(body, &mut b_offset)? as usize;
        if b_offset + len > body.len() {
            return None;
        }
        let ch_bytes = &body[b_offset..b_offset + len];
        b_offset += len;

        if field_num == 1 {
            // Channel info
            let mut c_offset = 0;
            let mut is_vqe0 = false;
            let mut pcm_data: Option<Vec<u8>> = None;

            while c_offset < ch_bytes.len() {
                let c_tag = read_varint(ch_bytes, &mut c_offset)?;
                let c_field = c_tag >> 3;
                let c_wire = c_tag & 7;

                match c_wire {
                    0 => {
                        let _ = read_varint(ch_bytes, &mut c_offset)?;
                    }
                    2 => {
                        let c_len = read_varint(ch_bytes, &mut c_offset)? as usize;
                        if c_offset + c_len > ch_bytes.len() {
                            break;
                        }
                        let val = &ch_bytes[c_offset..c_offset + c_len];
                        c_offset += c_len;

                        if c_field == 2 && val == b"vqe_0" {
                            is_vqe0 = true;
                        } else if c_field == 3 {
                            pcm_data = Some(val.to_vec());
                        }
                    }
                    _ => break,
                }
            }

            if is_vqe0 && pcm_data.is_some() {
                return pcm_data;
            }
        }
    }

    None
}

/// VQE IPC Audio capture client
pub struct VqeCapture {
    stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl VqeCapture {
    /// Start capturing 16kHz mono 16-bit PCM from yio_audio IPC (localhost:9850).
    /// Streams 512-byte PCM chunks into the provided sender channel.
    pub async fn start(pcm_tx: mpsc::Sender<Vec<u8>>) -> Result<Self> {
        info!("Connecting to yio_audio at {}...", YIO_AUDIO_ADDR);
        let mut stream = TcpStream::connect(YIO_AUDIO_ADDR)
            .await
            .context("Failed to connect to local yio_audio:9850")?;

        // Send request_vqe = 3
        let sub_pkt = build_ipc_request(3);
        stream.write_all(&sub_pkt).await?;
        info!("Subscribed to vqe_0 audio stream on Mini");

        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            let mut read_buf = VecDeque::new();
            let mut temp_buf = [0u8; 4096];

            loop {
                tokio::select! {
                    _ = &mut stop_rx => {
                        debug!("Stop signal received, sending unsubscribe to yio_audio...");
                        let unsub_pkt = build_ipc_request(6);
                        let _ = stream.write_all(&unsub_pkt).await;
                        break;
                    }
                    read_res = stream.read(&mut temp_buf) => {
                        match read_res {
                            Ok(0) => {
                                warn!("yio_audio stream closed by peer");
                                break;
                            }
                            Ok(n) => {
                                read_buf.extend(&temp_buf[..n]);

                                while read_buf.len() >= 4 {
                                    let mut len_bytes = [0u8; 4];
                                    for i in 0..4 {
                                        len_bytes[i] = read_buf[i];
                                    }
                                    let frame_len = u32::from_le_bytes(len_bytes) as usize;

                                    if frame_len > MAX_FRAME {
                                        error!("Frame length {} exceeds limit", frame_len);
                                        return;
                                    }

                                    if read_buf.len() < 4 + frame_len {
                                        break; // Wait for full frame
                                    }

                                    // Drop length prefix
                                    read_buf.drain(..4);
                                    let frame_payload: Vec<u8> = read_buf.drain(..frame_len).collect();

                                    if let Some(pcm) = extract_vqe_pcm(&frame_payload) {
                                        if pcm_tx.send(pcm).await.is_err() {
                                            debug!("Receiver channel closed, stopping VQE capture");
                                            return;
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Error reading from yio_audio: {}", e);
                                break;
                            }
                        }
                    }
                }
            }
        });

        Ok(Self {
            stop_tx: Some(stop_tx),
        })
    }

    /// Stop capturing and send unsubscribe to yio_audio
    pub fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for VqeCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_varint_roundtrip() {
        let test_values = [0, 1, 127, 128, 159, 253, 300, 16384, 100_000, u64::MAX / 2];
        for &val in &test_values {
            let encoded = varint(val);
            let mut offset = 0;
            let decoded = read_varint(&encoded, &mut offset).expect("decoding failed");
            assert_eq!(decoded, val);
            assert_eq!(offset, encoded.len());
        }
    }

    #[test]
    fn test_read_varint_truncated() {
        let buf = [0x80u8, 0x80u8];
        let mut offset = 0;
        assert_eq!(read_varint(&buf, &mut offset), None);
    }

    #[test]
    fn test_build_ipc_request_framing() {
        let req_pkt = build_ipc_request(3);
        assert!(req_pkt.len() > 4);
        let len_prefix =
            u32::from_le_bytes([req_pkt[0], req_pkt[1], req_pkt[2], req_pkt[3]]) as usize;
        assert_eq!(req_pkt.len() - 4, len_prefix);
    }

    #[test]
    fn test_extract_vqe_pcm_valid() {
        let pcm_payload = vec![0x12, 0x34, 0x56, 0x78];
        let mut ch_info = blob(2, b"vqe_0");
        ch_info.extend_from_slice(&blob(3, &pcm_payload));

        let body = blob(1, &ch_info);
        let mut payload = blob(252, &body);
        payload.extend_from_slice(&number(253, 158));

        let extracted = extract_vqe_pcm(&payload);
        assert_eq!(extracted, Some(pcm_payload));
    }

    #[test]
    fn test_extract_vqe_pcm_wrong_kind() {
        let mut ch_info = blob(2, b"vqe_0");
        ch_info.extend_from_slice(&blob(3, &[1, 2, 3]));
        let body = blob(1, &ch_info);
        let mut payload = blob(252, &body);
        payload.extend_from_slice(&number(253, 100));

        assert_eq!(extract_vqe_pcm(&payload), None);
    }

    #[test]
    fn test_extract_vqe_pcm_different_channel() {
        let mut ch_info = blob(2, b"raw_mic_0");
        ch_info.extend_from_slice(&blob(3, &[1, 2, 3]));
        let body = blob(1, &ch_info);
        let mut payload = blob(252, &body);
        payload.extend_from_slice(&number(253, 158));

        assert_eq!(extract_vqe_pcm(&payload), None);
    }
}
