//! Minimal GGUF v3 reader for the XLM-R classification head of
//! `bge-reranker-v2-m3` — the four tensors `cls.weight` / `cls.bias` /
//! `cls.output.weight` / `cls.output.bias`.
//!
//! ## Why this exists
//!
//! llama.cpp CAN score rerank models end-to-end via
//! `LLAMA_POOLING_TYPE_RANK`, but the pinned `llama-cpp-2 =0.1.151` (latest
//! as of 2026-07-06) sizes `embeddings_seq_ith`'s returned slice by
//! `n_embd` unconditionally, while Rank pooling stores exactly
//! `n_cls_out == 1` float per sequence — reading through the safe accessor
//! under Rank pooling is an out-of-bounds read. The reranker therefore runs
//! the encoder with CLS pooling (buffer correctly `n_embd`-sized) and
//! applies the classification head in Rust, which requires these weights.
//!
//! ## Scope
//!
//! NOT a general GGUF library: exactly enough header walking to locate
//! named tensors, plus `F32`/`F16` reads and `Q5_K`/`Q4_K` dequantization
//! (the types the bge conversions actually use — the staged Q5_K_M stores
//! `cls.weight` as Q5_K, biases + `cls.output.weight` as F32). Everything
//! else is a typed error naming the tensor and its ggml type.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// One parsed tensor-info record.
struct TensorInfo {
    dims: Vec<u64>,
    ggml_type: u32,
    offset: u64,
}

/// The dequantized XLM-R classification head. Row-major HF `Linear`
/// convention: `dense_w[j * in_dim + i]` is weight (out `j`, in `i`), so
/// `h_j = tanh(dense_b[j] + Σ_i cls_i · dense_w[j·in+i])` and
/// `logit = out_b + Σ_j h_j · out_w[j]`.
pub(crate) struct ClsHead {
    pub hidden: usize,
    pub dense_w: Vec<f32>,
    pub dense_b: Vec<f32>,
    pub out_w: Vec<f32>,
    pub out_b: f32,
}

#[derive(Debug, thiserror::Error)]
pub enum GgufHeadError {
    #[error("gguf io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a GGUF v2/v3 file: {0}")]
    Format(String),
    #[error(
        "tensor {name} missing from GGUF — was the model converted without its classification head? llama.cpp rerank conversions keep cls.*"
    )]
    Missing { name: String },
    #[error(
        "tensor {name} has unsupported ggml type {ggml_type} (supported: F32=0, F16=1, Q4_K=12, Q5_K=13)"
    )]
    UnsupportedType { name: String, ggml_type: u32 },
    #[error("tensor {name} has unexpected shape {dims:?}")]
    Shape { name: String, dims: Vec<u64> },
}

const GGML_F32: u32 = 0;
const GGML_F16: u32 = 1;
const GGML_Q4_K: u32 = 12;
const GGML_Q5_K: u32 = 13;
const QK_K: usize = 256;

pub(crate) fn read_cls_head(path: &Path) -> Result<ClsHead, GgufHeadError> {
    let mut f = std::fs::File::open(path)?;

    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)?;
    if &magic != b"GGUF" {
        return Err(GgufHeadError::Format("bad magic".into()));
    }
    let version = read_u32(&mut f)?;
    if !(2..=3).contains(&version) {
        return Err(GgufHeadError::Format(format!("unsupported version {version}")));
    }
    let tensor_count = read_u64(&mut f)?;
    let kv_count = read_u64(&mut f)?;

    // Walk the KV section only for `general.alignment`; skip everything else.
    let mut alignment: u64 = 32;
    for _ in 0..kv_count {
        let key = read_string(&mut f)?;
        let vtype = read_u32(&mut f)?;
        if key == "general.alignment" {
            alignment = read_kv_scalar_as_u64(&mut f, vtype)?;
        } else {
            skip_kv_value(&mut f, vtype)?;
        }
    }

    let mut infos: HashMap<String, TensorInfo> = HashMap::new();
    for _ in 0..tensor_count {
        let name = read_string(&mut f)?;
        let n_dims = read_u32(&mut f)?;
        let mut dims = Vec::with_capacity(n_dims as usize);
        for _ in 0..n_dims {
            dims.push(read_u64(&mut f)?);
        }
        let ggml_type = read_u32(&mut f)?;
        let offset = read_u64(&mut f)?;
        if name.starts_with("cls.") {
            infos.insert(name, TensorInfo { dims, ggml_type, offset });
        }
    }
    let header_end = f.stream_position()?;
    let data_start = header_end.div_ceil(alignment.max(1)) * alignment.max(1);

    let dense_w_info = take(&mut infos, "cls.weight")?;
    let dense_b_info = take(&mut infos, "cls.bias")?;
    let out_w_info = take(&mut infos, "cls.output.weight")?;
    let out_b_info = take(&mut infos, "cls.output.bias")?;

    // hidden size from the bias (1-D, unambiguous); dense must be square
    // hidden×hidden for XLM-R.
    let hidden = dense_b_info.dims.first().copied().unwrap_or(0) as usize;
    if hidden == 0 {
        return Err(GgufHeadError::Shape { name: "cls.bias".into(), dims: dense_b_info.dims });
    }
    if dense_w_info.dims != vec![hidden as u64, hidden as u64] {
        return Err(GgufHeadError::Shape { name: "cls.weight".into(), dims: dense_w_info.dims });
    }

    let dense_w = read_tensor(&mut f, data_start, "cls.weight", &dense_w_info, hidden * hidden)?;
    let dense_b = read_tensor(&mut f, data_start, "cls.bias", &dense_b_info, hidden)?;
    let out_w = read_tensor(&mut f, data_start, "cls.output.weight", &out_w_info, hidden)?;
    let out_b = read_tensor(&mut f, data_start, "cls.output.bias", &out_b_info, 1)?[0];

    Ok(ClsHead { hidden, dense_w, dense_b, out_w, out_b })
}

fn take(infos: &mut HashMap<String, TensorInfo>, name: &str) -> Result<TensorInfo, GgufHeadError> {
    infos.remove(name).ok_or_else(|| GgufHeadError::Missing { name: name.into() })
}

fn read_tensor(
    f: &mut std::fs::File,
    data_start: u64,
    name: &str,
    info: &TensorInfo,
    expected_elems: usize,
) -> Result<Vec<f32>, GgufHeadError> {
    let elems: u64 = info.dims.iter().product();
    if elems as usize != expected_elems {
        return Err(GgufHeadError::Shape { name: name.into(), dims: info.dims.clone() });
    }
    f.seek(SeekFrom::Start(data_start + info.offset))?;
    let n = elems as usize;
    match info.ggml_type {
        GGML_F32 => {
            let mut buf = vec![0u8; n * 4];
            f.read_exact(&mut buf)?;
            Ok(buf.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
        }
        GGML_F16 => {
            let mut buf = vec![0u8; n * 2];
            f.read_exact(&mut buf)?;
            Ok(buf.chunks_exact(2).map(|c| half_to_f32(u16::from_le_bytes([c[0], c[1]]))).collect())
        }
        GGML_Q5_K => {
            if !n.is_multiple_of(QK_K) {
                return Err(GgufHeadError::Shape { name: name.into(), dims: info.dims.clone() });
            }
            let blocks = n / QK_K;
            let mut buf = vec![0u8; blocks * 176]; // 2+2 halves + 12 scales + 32 qh + 128 qs
            f.read_exact(&mut buf)?;
            let mut out = Vec::with_capacity(n);
            for b in buf.chunks_exact(176) {
                dequant_q5_k_block(b, &mut out);
            }
            Ok(out)
        }
        GGML_Q4_K => {
            if !n.is_multiple_of(QK_K) {
                return Err(GgufHeadError::Shape { name: name.into(), dims: info.dims.clone() });
            }
            let blocks = n / QK_K;
            let mut buf = vec![0u8; blocks * 144]; // 2+2 halves + 12 scales + 128 qs
            f.read_exact(&mut buf)?;
            let mut out = Vec::with_capacity(n);
            for b in buf.chunks_exact(144) {
                dequant_q4_k_block(b, &mut out);
            }
            Ok(out)
        }
        other => Err(GgufHeadError::UnsupportedType { name: name.into(), ggml_type: other }),
    }
}

/// ggml `get_scale_min_k4` — unpack the 6-bit (scale, min) pair `j` of 8
/// from the 12-byte packed `scales` field.
fn scale_min_k4(j: usize, q: &[u8]) -> (f32, f32) {
    if j < 4 {
        ((q[j] & 63) as f32, (q[j + 4] & 63) as f32)
    } else {
        (
            ((q[j + 4] & 0x0F) | ((q[j - 4] >> 6) << 4)) as f32,
            ((q[j + 4] >> 4) | ((q[j] >> 6) << 4)) as f32,
        )
    }
}

/// ggml `dequantize_row_q5_K` for one 256-element block (176 bytes:
/// d: f16, dmin: f16, scales[12], qh[32], qs[128]).
fn dequant_q5_k_block(b: &[u8], out: &mut Vec<f32>) {
    let d = half_to_f32(u16::from_le_bytes([b[0], b[1]]));
    let dmin = half_to_f32(u16::from_le_bytes([b[2], b[3]]));
    let scales = &b[4..16];
    let qh = &b[16..48];
    let qs = &b[48..176];

    let mut is = 0usize;
    let mut u1: u8 = 1;
    let mut u2: u8 = 2;
    let mut ql = 0usize; // offset into qs, advances 32 per 64 elems
    for _ in (0..QK_K).step_by(64) {
        let (sc1, m1) = scale_min_k4(is, scales);
        let (d1, min1) = (d * sc1, dmin * m1);
        let (sc2, m2) = scale_min_k4(is + 1, scales);
        let (d2, min2) = (d * sc2, dmin * m2);
        for l in 0..32 {
            let q = (qs[ql + l] & 0x0F) as f32 + if qh[l] & u1 != 0 { 16.0 } else { 0.0 };
            out.push(d1 * q - min1);
        }
        for l in 0..32 {
            let q = (qs[ql + l] >> 4) as f32 + if qh[l] & u2 != 0 { 16.0 } else { 0.0 };
            out.push(d2 * q - min2);
        }
        ql += 32;
        is += 2;
        u1 <<= 2;
        u2 <<= 2;
    }
}

/// ggml `dequantize_row_q4_K` for one 256-element block (144 bytes:
/// d: f16, dmin: f16, scales[12], qs[128]).
fn dequant_q4_k_block(b: &[u8], out: &mut Vec<f32>) {
    let d = half_to_f32(u16::from_le_bytes([b[0], b[1]]));
    let dmin = half_to_f32(u16::from_le_bytes([b[2], b[3]]));
    let scales = &b[4..16];
    let qs = &b[16..144];

    let mut is = 0usize;
    let mut ql = 0usize;
    for _ in (0..QK_K).step_by(64) {
        let (sc1, m1) = scale_min_k4(is, scales);
        let (d1, min1) = (d * sc1, dmin * m1);
        let (sc2, m2) = scale_min_k4(is + 1, scales);
        let (d2, min2) = (d * sc2, dmin * m2);
        for l in 0..32 {
            out.push(d1 * (qs[ql + l] & 0x0F) as f32 - min1);
        }
        for l in 0..32 {
            out.push(d2 * (qs[ql + l] >> 4) as f32 - min2);
        }
        ql += 32;
        is += 2;
    }
}

/// IEEE 754 half → f32 (no `half` crate dependency for 4 tensors).
fn half_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) & 1) as u32;
    let exp = ((h >> 10) & 0x1F) as u32;
    let frac = (h & 0x3FF) as u32;
    let bits = match (exp, frac) {
        (0, 0) => sign << 31,
        (0, f) => {
            // subnormal: normalize
            let mut e = 127 - 15 + 1;
            let mut m = f;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            (sign << 31) | ((e as u32) << 23) | ((m & 0x3FF) << 13)
        }
        (0x1F, 0) => (sign << 31) | 0x7F80_0000,
        (0x1F, f) => (sign << 31) | 0x7F80_0000 | (f << 13),
        (e, f) => (sign << 31) | ((e + 127 - 15) << 23) | (f << 13),
    };
    f32::from_bits(bits)
}

// ---- header primitives ----

fn read_u32(f: &mut std::fs::File) -> Result<u32, std::io::Error> {
    let mut b = [0u8; 4];
    f.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(f: &mut std::fs::File) -> Result<u64, std::io::Error> {
    let mut b = [0u8; 8];
    f.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_string(f: &mut std::fs::File) -> Result<String, GgufHeadError> {
    let n = read_u64(f)?;
    let mut b = vec![0u8; n as usize];
    f.read_exact(&mut b)?;
    String::from_utf8(b).map_err(|e| GgufHeadError::Format(format!("non-utf8 string: {e}")))
}

fn kv_scalar_size(vtype: u32) -> Option<u64> {
    match vtype {
        0..=1 | 7 => Some(1),
        2..=3 => Some(2),
        4..=6 => Some(4),
        10..=12 => Some(8),
        _ => None,
    }
}

fn read_kv_scalar_as_u64(f: &mut std::fs::File, vtype: u32) -> Result<u64, GgufHeadError> {
    match vtype {
        4 => Ok(u64::from(read_u32(f)?)),
        10 => read_u64(f).map_err(Into::into),
        other => Err(GgufHeadError::Format(format!("general.alignment has kv type {other}"))),
    }
}

fn skip_kv_value(f: &mut std::fs::File, vtype: u32) -> Result<(), GgufHeadError> {
    if let Some(sz) = kv_scalar_size(vtype) {
        f.seek(SeekFrom::Current(sz as i64))?;
        return Ok(());
    }
    match vtype {
        8 => {
            let n = read_u64(f)?;
            f.seek(SeekFrom::Current(n as i64))?;
            Ok(())
        }
        9 => {
            let elem_type = read_u32(f)?;
            let count = read_u64(f)?;
            if let Some(sz) = kv_scalar_size(elem_type) {
                f.seek(SeekFrom::Current((sz * count) as i64))?;
            } else if elem_type == 8 {
                for _ in 0..count {
                    let n = read_u64(f)?;
                    f.seek(SeekFrom::Current(n as i64))?;
                }
            } else {
                return Err(GgufHeadError::Format(format!("nested array kv type {elem_type}")));
            }
            Ok(())
        }
        other => Err(GgufHeadError::Format(format!("unknown kv type {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_to_f32_reference_values() {
        assert_eq!(half_to_f32(0x0000), 0.0);
        assert_eq!(half_to_f32(0x3C00), 1.0);
        assert_eq!(half_to_f32(0xC000), -2.0);
        assert_eq!(half_to_f32(0x3555), 0.333_251_95); // ~1/3 in f16
        assert!(half_to_f32(0x7C00).is_infinite());
        // subnormal: smallest positive f16 = 2^-24
        assert!((half_to_f32(0x0001) - 2f32.powi(-24)).abs() < f32::EPSILON);
    }

    #[test]
    fn scale_min_k4_unpacks_low_and_high_groups() {
        // scales bytes crafted so group 0 → (63, 0) and group 4 exercises
        // the high-nibble reassembly path.
        let mut q = [0u8; 12];
        q[0] = 63; // sc0 low 6 bits
        q[4] = 0; // m0
        q[8] = 0x0F; // group 4 low nibble of sc
        let (sc0, m0) = scale_min_k4(0, &q);
        assert_eq!((sc0, m0), (63.0, 0.0));
        let (sc4, _) = scale_min_k4(4, &q);
        assert_eq!(sc4, 15.0); // low nibble only (q[0]>>6 == 0)
    }
}
