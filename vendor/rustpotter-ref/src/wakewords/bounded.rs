//! Mini-specific validation before allocating CBOR collections or creating a detector.

pub(crate) const MAX_REFERENCE_BYTES: usize = 1024 * 1024;

/// Accept only the small, definite-length CBOR subset used by WakewordRef.
/// The scan is allocation-free and rejects hostile size hints/depth before serde.
pub(crate) fn preflight_cbor(bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() > MAX_REFERENCE_BYTES {
        return Err("Invalid personal reference size".into());
    }
    let mut scan = Scanner {
        bytes,
        at: 0,
        nodes: 0,
    };
    scan.value(0)?;
    if scan.at != bytes.len() {
        return Err("Trailing data in personal reference".into());
    }
    Ok(())
}

struct Scanner<'a> {
    bytes: &'a [u8],
    at: usize,
    nodes: usize,
}

impl Scanner<'_> {
    fn take(&mut self, count: usize) -> Result<&[u8], String> {
        let end = self.at.checked_add(count).ok_or("CBOR length overflow")?;
        let value = self
            .bytes
            .get(self.at..end)
            .ok_or("Truncated CBOR reference")?;
        self.at = end;
        Ok(value)
    }

    fn argument(&mut self, info: u8) -> Result<u64, String> {
        let width = match info {
            0..=23 => return Ok(u64::from(info)),
            24 => 1,
            25 => 2,
            26 => 4,
            27 => 8,
            _ => return Err("Indefinite or reserved CBOR item".into()),
        };
        let mut value = 0_u64;
        for byte in self.take(width)? {
            value = (value << 8) | u64::from(*byte);
        }
        Ok(value)
    }

    fn value(&mut self, depth: usize) -> Result<(), String> {
        if depth > 8 || self.nodes >= 65_536 {
            return Err("Personal reference nesting or item budget exceeded".into());
        }
        self.nodes += 1;
        let head = self.take(1)?[0];
        let major = head >> 5;
        let info = head & 31;
        let argument = self.argument(info)?;
        match major {
            0 | 1 | 7 => Ok(()),
            2 | 3 => {
                if argument > 1024 {
                    return Err("Personal reference string is too long".into());
                }
                self.take(argument as usize)?;
                Ok(())
            }
            4 => {
                if argument > 512 {
                    return Err("Personal reference array is too long".into());
                }
                for _ in 0..argument {
                    self.value(depth + 1)?;
                }
                Ok(())
            }
            5 => {
                if argument > 32 {
                    return Err("Personal reference map is too large".into());
                }
                for _ in 0..argument {
                    self.value(depth + 1)?;
                    self.value(depth + 1)?;
                }
                Ok(())
            }
            _ => Err("Tagged CBOR is not supported in a personal reference".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unbounded_collections_and_trailing_data_before_decode() {
        for bytes in [
            vec![0x9b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            vec![0x9f, 0xff],
            vec![0xf6, 0xf6],
            vec![0x81; 12],
        ] {
            assert!(preflight_cbor(&bytes).is_err());
        }
        assert!(preflight_cbor(&[0xa1, 0x61, b'x', 0x82, 0x01, 0x02]).is_ok());
    }
}
