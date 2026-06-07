/// Extracts all length-delimited 32-byte values for a given field number from a protobuf blob.
fn collect_32byte_refs(data: &[u8], target_field: u64) -> Vec<[u8; 32]> {
    let mut pos = 0;
    let mut result = Vec::new();

    while pos < data.len() {
        let Some((tag, n)) = read_varint(&data[pos..]) else {
            break;
        };
        pos += n;

        let field_num = tag >> 3;
        let wire_type = (tag & 0x7) as u8;

        match wire_type {
            0 => {
                let Some((_, n)) = read_varint(&data[pos..]) else {
                    break;
                };
                pos += n;
            }
            1 => {
                if pos + 8 > data.len() {
                    break;
                }
                pos += 8;
            }
            2 => {
                let Some((len, n)) = read_varint(&data[pos..]) else {
                    break;
                };
                pos += n;
                let len = len as usize;
                if pos + len > data.len() {
                    break;
                }
                if field_num == target_field && len == 32 {
                    let mut hash = [0u8; 32];
                    hash.copy_from_slice(&data[pos..pos + 32]);
                    result.push(hash);
                }
                pos += len;
            }
            5 => {
                if pos + 4 > data.len() {
                    break;
                }
                pos += 4;
            }
            _ => break,
        }
    }

    result
}

/// Extracts all field-1 length-delimited 32-byte values from a protobuf binary blob.
/// These are the SHA256 IDs of message blobs, in order.
pub fn extract_field1_blobs(data: &[u8]) -> Vec<[u8; 32]> {
    collect_32byte_refs(data, 1)
}

/// Extracts all field-13 length-delimited 32-byte values from a protobuf binary blob.
/// These are the SHA256 IDs of pre-summary snapshot blobs, oldest first.
/// Field 13 is only present in root snapshot blobs that have undergone at least one
/// context summarization; one entry per summarization event.
pub fn extract_field13_refs(data: &[u8]) -> Vec<[u8; 32]> {
    collect_32byte_refs(data, 13)
}

/// Extracts the first occurrence of a length-delimited (wire type 2) field
/// with the given `target_field` number.  Returns None if absent.
pub fn extract_ld_field(data: &[u8], target_field: u64) -> Option<Vec<u8>> {
    let mut pos = 0;
    while pos < data.len() {
        let (tag, n) = read_varint(&data[pos..])?;
        pos += n;
        let field_num = tag >> 3;
        let wire_type = (tag & 0x7) as u8;
        match wire_type {
            0 => {
                let (_, n) = read_varint(&data[pos..])?;
                pos += n;
            }
            1 => {
                if pos + 8 > data.len() {
                    return None;
                }
                pos += 8;
            }
            2 => {
                let (len, n) = read_varint(&data[pos..])?;
                pos += n;
                let len = len as usize;
                if pos + len > data.len() {
                    return None;
                }
                if field_num == target_field {
                    return Some(data[pos..pos + len].to_vec());
                }
                pos += len;
            }
            5 => {
                if pos + 4 > data.len() {
                    return None;
                }
                pos += 4;
            }
            _ => return None,
        }
    }
    None
}

/// Extracts the field-4 length-delimited payload (inline partial assistant JSON).
pub fn extract_field4_bytes(data: &[u8]) -> Option<Vec<u8>> {
    extract_ld_field(data, 4)
}

/// Extracts field 9 (workspace URI, e.g. `file:///home/user/project`).
pub fn extract_field9_bytes(data: &[u8]) -> Option<Vec<u8>> {
    extract_ld_field(data, 9)
}

/// Extracts the first occurrence of a varint (wire type 0) field with the given field number.
fn extract_varint_field(data: &[u8], target_field: u64) -> Option<u64> {
    let mut pos = 0;
    while pos < data.len() {
        let (tag, n) = read_varint(&data[pos..])?;
        pos += n;
        let field_num = tag >> 3;
        let wire_type = (tag & 0x7) as u8;
        match wire_type {
            0 => {
                let (val, n) = read_varint(&data[pos..])?;
                pos += n;
                if field_num == target_field {
                    return Some(val);
                }
            }
            1 => {
                if pos + 8 > data.len() {
                    return None;
                }
                pos += 8;
            }
            2 => {
                let (len, n) = read_varint(&data[pos..])?;
                pos += n;
                let len = len as usize;
                if pos + len > data.len() {
                    return None;
                }
                pos += len;
            }
            5 => {
                if pos + 4 > data.len() {
                    return None;
                }
                pos += 4;
            }
            _ => return None,
        }
    }
    None
}

/// Extracts field 26 (snapshot timestamp in milliseconds since Unix epoch).
pub fn extract_field26_varint(data: &[u8]) -> Option<u64> {
    extract_varint_field(data, 26)
}

fn read_varint(data: &[u8]) -> Option<(u64, usize)> {
    let mut result = 0u64;
    let mut shift = 0u32;
    for (i, &b) in data.iter().enumerate() {
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some((result, i + 1));
        }
        shift += 7;
        if shift >= 64 {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_field1_blobs_empty() {
        assert!(extract_field1_blobs(&[]).is_empty());
    }

    #[test]
    fn test_extract_field1_single() {
        // field 1, wire type 2 (length-delimited), length 32
        let mut data = vec![0x0A, 0x20];
        let hash: [u8; 32] = (0..32u8).collect::<Vec<_>>().try_into().unwrap();
        data.extend_from_slice(&hash);
        let result = extract_field1_blobs(&data);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], hash);
    }

    #[test]
    fn test_extract_field1_two() {
        let mut data = vec![];
        let hash1: [u8; 32] = [0xAA; 32];
        let hash2: [u8; 32] = [0xBB; 32];
        for hash in [&hash1, &hash2] {
            data.push(0x0A);
            data.push(0x20);
            data.extend_from_slice(hash);
        }
        let result = extract_field1_blobs(&data);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], hash1);
        assert_eq!(result[1], hash2);
    }

    #[test]
    fn test_skip_non_field1() {
        // field 2, wire type 2, length 32 (should be skipped)
        let mut data = vec![0x12, 0x20];
        data.extend_from_slice(&[0xCC; 32]);
        // then field 1
        data.push(0x0A);
        data.push(0x20);
        let hash: [u8; 32] = [0xDD; 32];
        data.extend_from_slice(&hash);
        let result = extract_field1_blobs(&data);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], hash);
    }

    fn encode_varint_field(field_num: u64, value: u64) -> Vec<u8> {
        let tag = field_num << 3; // wire type 0
        let mut out = Vec::new();
        for v in [tag, value] {
            let mut v = v;
            loop {
                let byte = (v & 0x7F) as u8;
                v >>= 7;
                if v != 0 {
                    out.push(byte | 0x80);
                } else {
                    out.push(byte);
                    break;
                }
            }
        }
        out
    }

    fn encode_ld_field(field_num: u64, payload: &[u8]) -> Vec<u8> {
        let tag = (field_num << 3) | 2; // wire type 2
        let mut out = Vec::new();
        // encode tag varint
        let mut v = tag;
        loop {
            let byte = (v & 0x7F) as u8;
            v >>= 7;
            if v != 0 {
                out.push(byte | 0x80);
            } else {
                out.push(byte);
                break;
            }
        }
        // encode length varint
        let mut l = payload.len() as u64;
        loop {
            let byte = (l & 0x7F) as u8;
            l >>= 7;
            if l != 0 {
                out.push(byte | 0x80);
            } else {
                out.push(byte);
                break;
            }
        }
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn test_extract_field4_absent() {
        // Only a field-1 entry — no field 4.
        let mut data = vec![0x0A, 0x20];
        data.extend_from_slice(&[0x11; 32]);
        assert!(extract_field4_bytes(&data).is_none());
    }

    #[test]
    fn test_extract_field4_present() {
        let payload = b"hello field4";
        let data = encode_ld_field(4, payload);
        let result = extract_field4_bytes(&data);
        assert_eq!(result.as_deref(), Some(payload.as_ref()));
    }

    #[test]
    fn test_extract_field4_after_field1() {
        let hash: [u8; 32] = [0xAB; 32];
        let mut data = encode_ld_field(1, &hash);
        let payload = b"streaming json";
        data.extend(encode_ld_field(4, payload));
        let result = extract_field4_bytes(&data);
        assert_eq!(result.as_deref(), Some(payload.as_ref()));
    }

    #[test]
    fn test_extract_field13_refs_absent() {
        // Blob with only field-1 entries — field 13 should return empty.
        let hash: [u8; 32] = [0x11; 32];
        let data = encode_ld_field(1, &hash);
        assert!(extract_field13_refs(&data).is_empty());
    }

    #[test]
    fn test_extract_field13_refs_present() {
        let hash: [u8; 32] = [0xAA; 32];
        let data = encode_ld_field(13, &hash);
        let result = extract_field13_refs(&data);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], hash);
    }

    #[test]
    fn test_extract_field13_multiple() {
        let hash1: [u8; 32] = [0xAA; 32];
        let hash2: [u8; 32] = [0xBB; 32];
        let mut data = encode_ld_field(13, &hash1);
        data.extend(encode_ld_field(13, &hash2));
        let result = extract_field13_refs(&data);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], hash1);
        assert_eq!(result[1], hash2);
    }

    #[test]
    fn test_extract_field13_mixed_with_field1() {
        // Field 1 entries must not bleed into field-13 results.
        let f1_hash: [u8; 32] = [0x11; 32];
        let f13_hash: [u8; 32] = [0xCC; 32];
        let mut data = encode_ld_field(1, &f1_hash);
        data.extend(encode_ld_field(13, &f13_hash));
        data.extend(encode_ld_field(1, &f1_hash));
        let f1 = extract_field1_blobs(&data);
        let f13 = extract_field13_refs(&data);
        assert_eq!(f1.len(), 2);
        assert_eq!(f13.len(), 1);
        assert_eq!(f13[0], f13_hash);
    }

    #[test]
    fn test_extract_field26_absent() {
        // A blob with only field-1 entries — no field 26.
        let hash: [u8; 32] = [0x11; 32];
        let data = encode_ld_field(1, &hash);
        assert!(extract_field26_varint(&data).is_none());
    }

    #[test]
    fn test_extract_field26_present() {
        let ts_ms: u64 = 1_700_000_000_000;
        let data = encode_varint_field(26, ts_ms);
        assert_eq!(extract_field26_varint(&data), Some(ts_ms));
    }

    #[test]
    fn test_extract_field26_after_ld_fields() {
        // Field 26 appears after several length-delimited fields (realistic snapshot layout).
        let hash: [u8; 32] = [0xAB; 32];
        let mut data = encode_ld_field(1, &hash);
        data.extend(encode_ld_field(9, b"file:///home/user/project"));
        let ts_ms: u64 = 1_777_000_000_000;
        data.extend(encode_varint_field(26, ts_ms));
        assert_eq!(extract_field26_varint(&data), Some(ts_ms));
    }

    #[test]
    fn test_extract_field26_returns_first() {
        // Only the first occurrence should be returned.
        let mut data = encode_varint_field(26, 111);
        data.extend(encode_varint_field(26, 222));
        assert_eq!(extract_field26_varint(&data), Some(111));
    }
}
