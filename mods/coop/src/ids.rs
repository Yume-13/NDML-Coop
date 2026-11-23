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

//! Instance ids as numbers.

#![forbid(unsafe_code)]

use ndml_sdk::Value;

/// An instance id as a number (ids come back as refs or reals; small, so the casts are exact).
#[must_use]
pub fn id_number(v: &Value) -> Option<i64> {
    match v {
        Value::Ref(p) => Some((*p & 0xFFFF_FFFF) as i64),
        other => other.as_f64().filter(|n| n.is_finite()).map(|n| n as i64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_as_numbers() {
        assert_eq!(id_number(&Value::Real(100_123.0)), Some(100_123));
        assert_eq!(id_number(&Value::Ref((1 << 32) | 0x4d)), Some(77));
    }
}
