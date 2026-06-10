use std::io::Cursor;

use bytes::BytesMut;

use crate::errors::Error;

/// Postgres data type mappings
/// used in RowDescription ('T') message.
pub enum DataType {
    Text,
    Int4,
    Numeric,
    Bool,
    Oid,
    AnyArray,
    Any,
}

impl From<&DataType> for i32 {
    fn from(data_type: &DataType) -> i32 {
        match data_type {
            DataType::Text => 25,
            DataType::Int4 => 23,
            DataType::Numeric => 1700,
            DataType::Bool => 16,
            DataType::Oid => 26,
            DataType::AnyArray => 2277,
            DataType::Any => 2276,
        }
    }
}

/// Trait for reading strings from BytesMut
pub trait BytesMutReader {
    fn read_string(&mut self) -> Result<String, Error>;
}

impl BytesMutReader for Cursor<&BytesMut> {
    /// Should only be used when reading strings from the message protocol.
    /// Can be used to read multiple strings from the same message which are separated by the null byte.
    ///
    /// `read_until` returns `Ok(0)` at EOF without finding the delimiter
    /// and leaves `buf` empty. Computing `buf.len() - 1` on an empty buffer
    /// underflows usize and panics. Treat "no nul terminator before EOF" as
    /// a protocol error instead. Reachable from `Parse::try_from`,
    /// `Bind::try_from`, `Describe::try_from`, `Close::try_from` on malformed
    /// extended-protocol messages - without this guard a panic propagates
    /// through tokio's task abort and (via the panic hook) used to terminate
    /// the entire pooler.
    fn read_string(&mut self) -> Result<String, Error> {
        // scan the borrowed buffer slice in place for the nul
        // terminator and build a single String - instead of read_until,
        // which first copies the bytes (including the nul) into a temporary
        // Vec and then allocates a second String from it. Error semantics
        // are preserved byte-for-byte:
        //   * already at EOF    -> "Empty string field at end of buffer"
        //   * no nul before EOF -> "Unterminated string ..."
        //   * invalid UTF-8     -> "Invalid UTF-8 string ..."
        let buf = *self.get_ref();
        let start = self.position() as usize;
        if start >= buf.len() {
            return Err(Error::ParseBytesError(
                "Empty string field at end of buffer".to_string(),
            ));
        }
        let rest = &buf[start..];
        match rest.iter().position(|&byte| byte == b'\0') {
            None => Err(Error::ParseBytesError(
                "Unterminated string in extended-protocol message".to_string(),
            )),
            Some(nul_offset) => {
                let value = std::str::from_utf8(&rest[..nul_offset])
                    .map(|value| value.to_string())
                    .map_err(|err| {
                        Error::ParseBytesError(format!(
                            "Invalid UTF-8 string in extended-protocol message: {err}"
                        ))
                    })?;
                // Advance past the string and its nul terminator, exactly as
                // read_until would have.
                self.set_position((start + nul_offset + 1) as u64);
                Ok(value)
            }
        }
    }
}

impl BytesMutReader for BytesMut {
    /// Should only be used when reading strings from the message protocol.
    /// Can be used to read multiple strings from the same message which are separated by the null byte
    fn read_string(&mut self) -> Result<String, Error> {
        let null_index = self.iter().position(|&byte| byte == b'\0');

        match null_index {
            Some(index) => {
                let string_bytes = self.split_to(index + 1);
                std::str::from_utf8(&string_bytes[..string_bytes.len() - 1])
                    .map(|value| value.to_string())
                    .map_err(|err| {
                        Error::ParseBytesError(format!(
                            "Invalid UTF-8 string in extended-protocol message: {err}"
                        ))
                    })
            }
            None => Err(Error::ParseBytesError("Could not read string".to_string())),
        }
    }
}

/// Convert a vector of bytes to a string.
pub fn vec_to_string(vec: Vec<u8>) -> Result<String, Error> {
    let vec_with_nul = match std::str::from_utf8(&vec) {
        Ok(token) => token,
        Err(err) => return Err(Error::ConvertError(err.to_string())),
    };
    match std::ffi::CStr::from_bytes_until_nul(vec_with_nul.as_ref()) {
        Ok(token) => Ok(token.to_str().unwrap().to_string()),
        Err(err) => Err(Error::ConvertError(err.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BufMut;

    fn cursor_buf(bytes: &[u8]) -> BytesMut {
        let mut b = BytesMut::new();
        b.put_slice(bytes);
        b
    }

    #[test]
    fn cursor_read_string_reads_multiple_fields_and_advances() {
        // Two nul-terminated fields back to back, exactly like a Bind
        // portal+statement pair.
        let buf = cursor_buf(b"portal\0stmt\0");
        let mut cursor = Cursor::new(&buf);
        assert_eq!(cursor.read_string().unwrap(), "portal");
        assert_eq!(cursor.read_string().unwrap(), "stmt");
        // Cursor is now at EOF.
        assert!(cursor.read_string().is_err());
    }

    #[test]
    fn cursor_read_string_empty_field() {
        // A lone nul is a valid empty string (anonymous statement name).
        let buf = cursor_buf(b"\0");
        let mut cursor = Cursor::new(&buf);
        assert_eq!(cursor.read_string().unwrap(), "");
    }

    #[test]
    fn cursor_read_string_at_eof_is_error() {
        let buf = cursor_buf(b"");
        let mut cursor = Cursor::new(&buf);
        let err = cursor.read_string().unwrap_err();
        assert!(format!("{err:?}").contains("Empty string field at end of buffer"));
    }

    #[test]
    fn cursor_read_string_unterminated_is_error() {
        let buf = cursor_buf(b"no nul here");
        let mut cursor = Cursor::new(&buf);
        let err = cursor.read_string().unwrap_err();
        assert!(format!("{err:?}").contains("Unterminated string"));
    }

    #[test]
    fn cursor_read_string_invalid_utf8_is_error() {
        let buf = cursor_buf(&[0xff, 0xfe, 0]);
        let mut cursor = Cursor::new(&buf);
        let err = cursor.read_string().unwrap_err();
        assert!(format!("{err:?}").contains("Invalid UTF-8 string"));
    }
}
