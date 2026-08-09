//! Minimal PNG reader: extracts IHDR dimensions and text chunks (tEXt/zTXt/iTXt)
//! without decoding pixel data. Non-text chunks are skipped via seek, so reading
//! metadata from a multi-megabyte PNG touches only a few kilobytes.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use flate2::read::ZlibDecoder;

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
const MAX_TEXT_CHUNK_LEN: u32 = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct TextChunk {
    pub keyword: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct PngHeader {
    pub width: u32,
    pub height: u32,
    pub text_chunks: Vec<TextChunk>,
}

pub fn read_png_header(path: &Path) -> Result<PngHeader, String> {
    let file = File::open(path).map_err(|e| format!("open failed: {e}"))?;
    let mut reader = BufReader::new(file);

    let mut signature = [0u8; 8];
    reader
        .read_exact(&mut signature)
        .map_err(|e| format!("read failed: {e}"))?;
    if signature != PNG_SIGNATURE {
        return Err("not a PNG file".to_string());
    }

    let mut width = 0u32;
    let mut height = 0u32;
    let mut text_chunks: Vec<TextChunk> = Vec::new();

    loop {
        let mut chunk_head = [0u8; 8];
        if reader.read_exact(&mut chunk_head).is_err() {
            break; // tolerate truncated files: keep whatever was read so far
        }
        let length = u32::from_be_bytes([chunk_head[0], chunk_head[1], chunk_head[2], chunk_head[3]]);
        let chunk_type: [u8; 4] = [chunk_head[4], chunk_head[5], chunk_head[6], chunk_head[7]];

        match &chunk_type {
            b"IHDR" => {
                if length < 8 {
                    return Err("malformed IHDR".to_string());
                }
                let mut data = vec![0u8; length as usize];
                reader
                    .read_exact(&mut data)
                    .map_err(|e| format!("bad IHDR: {e}"))?;
                width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
                skip_bytes(&mut reader, 4)?;
            }
            b"tEXt" | b"zTXt" | b"iTXt" => {
                if length > MAX_TEXT_CHUNK_LEN {
                    return Err(format!("text chunk too large: {length} bytes"));
                }
                let mut data = vec![0u8; length as usize];
                reader
                    .read_exact(&mut data)
                    .map_err(|e| format!("bad text chunk: {e}"))?;
                if let Some(chunk) = parse_text_chunk(&chunk_type, &data) {
                    text_chunks.push(chunk);
                }
                skip_bytes(&mut reader, 4)?;
            }
            b"IEND" => break,
            _ => {
                skip_bytes(&mut reader, length as i64 + 4)?;
            }
        }
    }

    Ok(PngHeader {
        width,
        height,
        text_chunks,
    })
}

fn skip_bytes(reader: &mut BufReader<File>, count: i64) -> Result<(), String> {
    reader
        .seek_relative(count)
        .map_err(|e| format!("seek failed: {e}"))
}

fn parse_text_chunk(chunk_type: &[u8; 4], data: &[u8]) -> Option<TextChunk> {
    match chunk_type {
        b"tEXt" => {
            let sep = data.iter().position(|&b| b == 0)?;
            Some(TextChunk {
                keyword: decode_text(&data[..sep]),
                text: decode_text(&data[sep + 1..]),
            })
        }
        b"zTXt" => {
            // keyword \0 compression-method(1) zlib-data
            let sep = data.iter().position(|&b| b == 0)?;
            let compressed = data.get(sep + 2..)?;
            Some(TextChunk {
                keyword: decode_text(&data[..sep]),
                text: decode_text(&inflate(compressed)?),
            })
        }
        b"iTXt" => {
            // keyword \0 comp-flag(1) comp-method(1) language \0 translated-keyword \0 text
            let sep = data.iter().position(|&b| b == 0)?;
            let keyword = decode_text(&data[..sep]);
            let rest = data.get(sep + 1..)?;
            let compression_flag = *rest.first()?;
            let after_flags = rest.get(2..)?;
            let lang_end = after_flags.iter().position(|&b| b == 0)?;
            let after_lang = after_flags.get(lang_end + 1..)?;
            let trans_end = after_lang.iter().position(|&b| b == 0)?;
            let payload = after_lang.get(trans_end + 1..)?;
            let text = if compression_flag == 1 {
                decode_text(&inflate(payload)?)
            } else {
                decode_text(payload)
            };
            Some(TextChunk { keyword, text })
        }
        _ => None,
    }
}

fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    _ = ZlibDecoder::new(data).read_to_end(&mut out).ok()?;
    Some(out)
}

/// UTF-8 first (what ComfyUI and similar tools actually write), Latin-1
/// fallback (what the PNG spec mandates for tEXt).
fn decode_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}
