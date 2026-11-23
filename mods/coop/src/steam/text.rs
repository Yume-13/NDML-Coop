// MIT License
//
// Copyright © 2026 Yume-13
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! Plain-text helpers: persona names made safe to draw and store, base64 for
//! the bulk buffer route, and the `+connect_lobby` launch argument.

#![forbid(unsafe_code)]

/// Longest persona name kept (characters).
pub const MAX_PERSONA_CHARS: usize = 32;

/// A persona name as plain text: invisible and control characters removed,
/// whitespace collapsed, at most [`MAX_PERSONA_CHARS`]; `"?"` if empty.
#[must_use]
pub fn clean_persona(raw: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in raw.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if c.is_control() || is_invisible(c) {
            continue;
        }
        if out.chars().count() >= MAX_PERSONA_CHARS {
            break;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c);
    }
    if out.is_empty() { "?".into() } else { out }
}

/// Format controls, private-use areas, specials and variation selectors.
fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}'
        | '\u{034F}'
        | '\u{061C}'
        | '\u{115F}'..='\u{1160}'
        | '\u{17B4}'..='\u{17B5}'
        | '\u{180B}'..='\u{180F}'
        | '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}'
        | '\u{3164}'
        | '\u{E000}'..='\u{F8FF}'
        | '\u{FE00}'..='\u{FE0F}'
        | '\u{FEFF}'
        | '\u{FFA0}'
        | '\u{FFF0}'..='\u{FFFF}'
        | '\u{E0000}'..='\u{E0FFF}'
        | '\u{F0000}'..='\u{10FFFF}')
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding (what `buffer_base64_decode_ext` reads).
#[must_use]
pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(B64[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn b64_value(c: u8) -> Option<u32> {
    Some(u32::from(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    }))
}

/// Decode what `buffer_base64_encode` returns (`None` on a bad character).
#[must_use]
pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        acc = (acc << 6) | b64_value(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// The lobby id from Steam's `+connect_lobby <id>` launch argument (an
/// invite accepted while the game was closed).
#[must_use]
pub fn connect_lobby_arg<S: AsRef<str>>(args: &[S]) -> Option<u64> {
    let mut it = args.iter().map(AsRef::as_ref);
    while let Some(a) = it.next() {
        let value = if a.eq_ignore_ascii_case("+connect_lobby") {
            it.next()
        } else if let Some((k, v)) = a.split_once('=') {
            k.eq_ignore_ascii_case("+connect_lobby").then_some(v)
        } else {
            None
        };
        if let Some(v) = value {
            return v.trim().parse::<u64>().ok().filter(|&id| id != 0);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persona_names_become_plain_text() {
        assert_eq!(clean_persona("  Ace  "), "Ace");
        assert_eq!(clean_persona("Wing\u{202E}gniW"), "WinggniW");
        assert_eq!(clean_persona("a\u{200B}b\u{FEFF}c"), "abc");
        assert_eq!(clean_persona("tab\tand\nnewline"), "tab and newline");
        assert_eq!(clean_persona("many    spaces"), "many spaces");
        assert_eq!(clean_persona("\u{7}\u{1b}[31m"), "[31m");
        assert_eq!(clean_persona(""), "?");
        assert_eq!(clean_persona("\u{200B}\u{3164}"), "?");
        assert_eq!(clean_persona("ÅsaÑoño 名前"), "ÅsaÑoño 名前");
        let long = clean_persona(&"x".repeat(100));
        assert_eq!(long.chars().count(), MAX_PERSONA_CHARS);
    }

    #[test]
    fn base64_round_trips() {
        for len in [0usize, 1, 2, 3, 4, 40, 1199, 1200] {
            let d: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let s = b64_encode(&d);
            assert_eq!(s.len(), len.div_ceil(3) * 4);
            assert_eq!(b64_decode(&s).unwrap(), d);
        }
        assert_eq!(b64_encode(b"Man"), "TWFu");
        assert_eq!(b64_encode(b"Ma"), "TWE=");
        assert_eq!(b64_decode("TWE="), Some(b"Ma".to_vec()));
        assert_eq!(b64_decode("T!E="), None);
    }

    #[test]
    fn connect_lobby_from_the_command_line() {
        assert_eq!(
            connect_lobby_arg(&["NovaDrift.exe", "+connect_lobby", "109775241000000001"]),
            Some(109_775_241_000_000_001)
        );
        assert_eq!(connect_lobby_arg(&["x", "+CONNECT_LOBBY=42"]), Some(42));
        assert_eq!(connect_lobby_arg(&["x", "+connect_lobby"]), None);
        assert_eq!(connect_lobby_arg(&["x", "+connect_lobby", "abc"]), None);
        assert_eq!(connect_lobby_arg(&["x", "+connect_lobby", "0"]), None);
        assert_eq!(connect_lobby_arg(&["x", "-windowed"]), None);
        assert_eq!(connect_lobby_arg::<&str>(&[]), None);
    }
}
