//! Modified version of https://github.com/tower-rs/tower-http/blob/main/tower-http/src/content_encoding.rs

// based on https://github.com/http-rs/accept-encoding
pub fn encodings(headers: &http::HeaderMap) -> impl Iterator<Item = (Encoding, QValue)> + '_ {
    headers
        .get_all(http::header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|hval| hval.to_str().ok())
        .flat_map(|s| s.split(','))
        .filter_map(move |v| {
            let mut v = v.splitn(2, ';');

            let encoding = match Encoding::parse(v.next().unwrap().trim()) {
                Some(encoding) => encoding,
                None => return None, // ignore unknown encodings
            };

            let qval = if let Some(qval) = v.next() {
                QValue::parse(qval.trim())?
            } else {
                QValue::one()
            };

            Some((encoding, qval))
        })
}

// This enum's variants are ordered from least to most preferred.
#[derive(Copy, Clone, Debug, Ord, PartialOrd, PartialEq, Eq)]
pub(crate) enum Encoding {
    #[allow(dead_code)]
    Identity,
    Gzip,
    Brotli,
}

impl Encoding {
    fn to_str(self) -> &'static str {
        match self {
            Encoding::Gzip => "gzip",
            Encoding::Brotli => "br",
            Encoding::Identity => "identity",
        }
    }

    pub fn to_file_extension(self) -> Option<&'static std::ffi::OsStr> {
        match self {
            Encoding::Gzip => Some(std::ffi::OsStr::new(".gz")),
            Encoding::Brotli => Some(std::ffi::OsStr::new(".br")),
            Encoding::Identity => None,
        }
    }

    pub fn into_header_value(self) -> http::HeaderValue {
        http::HeaderValue::from_static(self.to_str())
    }

    fn parse(s: &str) -> Option<Encoding> {
        if s.eq_ignore_ascii_case("gzip") || s.eq_ignore_ascii_case("x-gzip") {
            return Some(Encoding::Gzip);
        }

        if s.eq_ignore_ascii_case("br") {
            return Some(Encoding::Brotli);
        }

        if s.eq_ignore_ascii_case("identity") {
            return Some(Encoding::Identity);
        }

        None
    }
}

// Allowed q-values are numbers between 0 and 1 with at most 3 digits in the fractional part. They
// are presented here as an unsigned integer between 0 and 1000.

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct QValue(pub u16);

impl QValue {
    #[inline]
    pub(crate) fn one() -> Self {
        Self(1000)
    }

    // Parse a q-value as specified in RFC 7231 section 5.3.1.
    fn parse(s: &str) -> Option<Self> {
        let mut c = s.chars();
        // Parse "q=" (case-insensitively).
        match c.next() {
            Some('q' | 'Q') => (),
            _ => return None,
        };
        match c.next() {
            Some('=') => (),
            _ => return None,
        };

        // Parse leading digit. Since valid q-values are between 0.000 and 1.000, only "0" and "1"
        // are allowed.
        let mut value = match c.next() {
            Some('0') => 0,
            Some('1') => 1000,
            _ => return None,
        };

        // Parse optional decimal point.
        match c.next() {
            Some('.') => (),
            None => return Some(Self(value)),
            _ => return None,
        };

        // Parse optional fractional digits. The value of each digit is multiplied by `factor`.
        // Since the q-value is represented as an integer between 0 and 1000, `factor` is `100` for
        // the first digit, `10` for the next, and `1` for the digit after that.
        let mut factor = 100;
        loop {
            match c.next() {
                Some(n @ '0'..='9') => {
                    // If `factor` is less than `1`, three digits have already been parsed. A
                    // q-value having more than 3 fractional digits is invalid.
                    if factor < 1 {
                        return None;
                    }
                    // Add the digit's value multiplied by `factor` to `value`.
                    value += factor * (n as u16 - '0' as u16);
                }
                None => {
                    // No more characters to parse. Check that the value representing the q-value is
                    // in the valid range.
                    return if value <= 1000 {
                        Some(Self(value))
                    } else {
                        None
                    };
                }
                _ => return None,
            };
            factor /= 10;
        }
    }
}
