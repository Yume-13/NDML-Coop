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

//! Steam room codes: five characters of Crockford base32 (no `I L O U`),
//! carried as lobby data `code` and found with a lobby search. Case, spaces
//! and dashes are ignored; `I`/`L` read as `1` and `O` as `0`.

#![forbid(unsafe_code)]

use ndml_net::rng::Rng;

/// Characters in a room code.
pub const LEN: usize = 5;
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A fresh random room code.
#[must_use]
pub fn generate(rng: &mut Rng) -> String {
    (0..LEN)
        .map(|_| char::from(ALPHABET[rng.below(32) as usize]))
        .collect()
}

/// A typed code in canonical form, or why it isn't one (for the player).
pub fn normalize(input: &str) -> Result<String, String> {
    let mut out = String::new();
    for (i, c) in input.chars().enumerate() {
        if c.is_whitespace() || c == '-' {
            continue;
        }
        let c = match c.to_ascii_uppercase() {
            'I' | 'L' => '1',
            'O' => '0',
            c => c,
        };
        if !c.is_ascii() || !ALPHABET.contains(&(c as u8)) {
            return Err(format!(
                "'{c}' (character {}) is not used in room codes",
                i + 1
            ));
        }
        out.push(c);
    }
    match out.len() {
        0 => Err("type the host's room code".into()),
        n if n == LEN => Ok(out),
        n => Err(format!("a room code has {LEN} characters, this has {n}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_codes_are_valid_and_vary() {
        let mut rng = Rng::new(7);
        let codes: Vec<String> = (0..50).map(|_| generate(&mut rng)).collect();
        for c in &codes {
            assert_eq!(c.len(), LEN);
            assert_eq!(normalize(c).as_deref(), Ok(c.as_str()));
        }
        let distinct: std::collections::BTreeSet<_> = codes.iter().collect();
        assert!(distinct.len() > 45);
    }

    #[test]
    fn typing_is_forgiving_and_errors_say_why() {
        assert_eq!(normalize(" k7q-x2 ").as_deref(), Ok("K7QX2"));
        assert_eq!(normalize("ilo00").as_deref(), Ok("11000"));
        assert!(normalize("K7QX").unwrap_err().contains("has 4"));
        assert!(normalize("K7QXU").unwrap_err().contains("'U'"));
        assert!(normalize("").unwrap_err().contains("type"));
        assert!(normalize("K7QX2Z").is_err());
        assert!(normalize("K7QÉ2").is_err());
    }
}
