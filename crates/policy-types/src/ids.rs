use core::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HexError;

macro_rules! hex_id {
    ($name:ident, $n:expr) => {
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub struct $name(pub [u8; $n]);

        impl $name {
            pub const fn new(bytes: [u8; $n]) -> Self {
                Self(bytes)
            }

            pub fn from_hex(s: &str) -> Result<Self, HexError> {
                Ok(Self(parse_hex_array(s)?))
            }

            pub fn write_hex(self, dst: &mut [u8]) -> Result<(), HexError> {
                if dst.len() < $n * 2 {
                    return Err(HexError);
                }
                encode_hex(&self.0, &mut dst[..$n * 2]);
                Ok(())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in self.0 {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in self.0 {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }
    };
}

hex_id!(BootId, 16);
hex_id!(SessionId, 16);
hex_id!(Hash32, 32);

impl Hash32 {
    pub const fn repeat(byte: u8) -> Self {
        Self([byte; 32])
    }
}

pub fn parse_hex_array<const N: usize>(s: &str) -> Result<[u8; N], HexError> {
    let bytes = s.as_bytes();
    if bytes.len() != N * 2 {
        return Err(HexError);
    }
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N {
        let hi = hex_val(bytes[i * 2])?;
        let lo = hex_val(bytes[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    Ok(out)
}

pub fn encode_hex(src: &[u8], dst: &mut [u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (i, byte) in src.iter().enumerate() {
        dst[i * 2] = HEX[(byte >> 4) as usize];
        dst[i * 2 + 1] = HEX[(byte & 0xf) as usize];
    }
}

fn hex_val(b: u8) -> Result<u8, HexError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        _ => Err(HexError),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_uppercase_and_odd_lengths() {
        assert!(BootId::from_hex("0123456789ABCDEF0123456789ABCDEF").is_err());
        assert!(BootId::from_hex("abcd").is_err());
        let id = BootId::from_hex("0123456789abcdef0123456789abcdef").unwrap();
        let mut buf = [0u8; 32];
        id.write_hex(&mut buf).unwrap();
        assert_eq!(
            core::str::from_utf8(&buf).unwrap(),
            "0123456789abcdef0123456789abcdef"
        );
    }
}
