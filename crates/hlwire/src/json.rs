//! Fast `/exchange` body parsing: JSON text -> `UserTx` with the action
//! transcoded straight to msgpack (no `serde_json::Value`).
//!
//! The fast path only ever *succeeds* on input that `serde_json` accepts and
//! for which it produces exactly what `serde_json::from_slice` +
//! [`crate::parse_exchange_request`] would. On anything unusual (escapes in
//! keys, duplicate keys, floats, deep nesting, any error, ...) it gives up and
//! the caller runs the reference path, so results and error messages are
//! identical by construction. `tests/props.rs` checks the equivalence.

use crate::decode::parse_address;
use crate::msgpack::{write_int, write_str, write_uint};
use crate::sign::parse_signature_parts;
use interfaces::{Address, UserTx};

const MAX_DEPTH: usize = 64;
const MAX_KEYS: usize = 32;

struct J<'a> {
    b: &'a [u8],
    p: usize,
    /// Keys of the objects currently being transcoded (stack; for the
    /// duplicate-key check), reused across objects to avoid allocation.
    keys: Vec<&'a [u8]>,
}

/// `None` = give up (use the reference path).
type O<T> = Option<T>;

struct StrTok<'a> {
    raw: &'a [u8],
    s: &'a str,
    escaped: bool,
}

impl<'a> J<'a> {
    #[inline(always)]
    fn ws(&mut self) {
        while let Some(&c) = self.b.get(self.p) {
            if c == b' ' || c == b'\n' || c == b'\t' || c == b'\r' {
                self.p += 1;
            } else {
                break;
            }
        }
    }

    #[inline(always)]
    fn peek(&self) -> O<u8> {
        self.b.get(self.p).copied()
    }

    #[inline(always)]
    fn eat(&mut self, c: u8) -> O<()> {
        if self.peek()? == c {
            self.p += 1;
            Some(())
        } else {
            None
        }
    }

    fn lit(&mut self, s: &[u8]) -> O<()> {
        if self.b.get(self.p..self.p + s.len())? == s {
            self.p += s.len();
            Some(())
        } else {
            None
        }
    }

    /// String token (cursor at the opening quote). Validates UTF-8 and
    /// escapes; control characters are rejected (as serde_json does).
    fn string(&mut self) -> O<StrTok<'a>> {
        self.eat(b'"')?;
        let start = self.p;
        let mut escaped = false;
        let b = self.b;
        let mut i = start;
        loop {
            // tight scan to the next special byte
            while let Some(&c) = b.get(i) {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                i += 1;
            }
            match *b.get(i)? {
                b'"' => break,
                b'\\' => {
                    escaped = true;
                    i += 1;
                    match *b.get(i)? {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => i += 1,
                        b'u' => {
                            // Validated precisely when unescaping.
                            let h = b.get(i + 1..i + 5)?;
                            if !h.iter().all(u8::is_ascii_hexdigit) {
                                return None;
                            }
                            i += 5;
                        }
                        _ => return None,
                    }
                }
                _ => return None, // control character
            }
        }
        let raw = std::str::from_utf8(&b[start..i]).ok()?;
        self.p = i + 1;
        if escaped {
            // full escape validation (e.g. lone surrogates), as serde_json
            unescape(raw.as_bytes())?;
        }
        Some(StrTok { raw: raw.as_bytes(), s: raw, escaped })
    }

    /// Number token: (slice, is_float). Strict JSON grammar.
    fn number(&mut self) -> O<(&'a [u8], bool)> {
        let start = self.p;
        let mut float = false;
        if self.peek()? == b'-' {
            self.p += 1;
        }
        match self.peek()? {
            b'0' => self.p += 1,
            b'1'..=b'9' => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.p += 1;
                }
            }
            _ => return None,
        }
        if self.peek() == Some(b'.') {
            float = true;
            self.p += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return None;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.p += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            float = true;
            self.p += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.p += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return None;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.p += 1;
            }
        }
        Some((&self.b[start..self.p], float))
    }

    fn uint_token(&mut self) -> O<u64> {
        let (t, float) = self.number()?;
        if float || t[0] == b'-' {
            return None;
        }
        std::str::from_utf8(t).ok()?.parse().ok()
    }

    /// Validate and skip any value.
    fn skip(&mut self, depth: usize) -> O<()> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.ws();
        match self.peek()? {
            b'"' => {
                self.string()?;
            }
            b'{' => {
                self.p += 1;
                self.ws();
                if self.peek()? == b'}' {
                    self.p += 1;
                    return Some(());
                }
                loop {
                    self.ws();
                    self.string()?;
                    self.ws();
                    self.eat(b':')?;
                    self.skip(depth + 1)?;
                    self.ws();
                    match self.peek()? {
                        b',' => self.p += 1,
                        b'}' => {
                            self.p += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
            }
            b'[' => {
                self.p += 1;
                self.ws();
                if self.peek()? == b']' {
                    self.p += 1;
                    return Some(());
                }
                loop {
                    self.skip(depth + 1)?;
                    self.ws();
                    match self.peek()? {
                        b',' => self.p += 1,
                        b']' => {
                            self.p += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
            }
            b't' => self.lit(b"true")?,
            b'f' => self.lit(b"false")?,
            b'n' => self.lit(b"null")?,
            _ => {
                self.number()?;
            }
        }
        Some(())
    }

    /// Transcode one JSON value to msgpack (cursor before the value).
    /// `type_seen` is set when this is an object with a string `"type"`.
    fn transcode(&mut self, out: &mut Vec<u8>, depth: usize, type_seen: &mut bool) -> O<()> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.ws();
        match self.peek()? {
            b'"' => {
                let s = self.string()?;
                if s.escaped {
                    write_str(out, &unescape(s.raw)?);
                } else {
                    // validated UTF-8 above
                    write_str(out, s.s);
                }
            }
            b'{' => {
                self.p += 1;
                let hdr = out.len();
                out.push(0x80);
                let base = self.keys.len();
                let mut n = 0usize;
                self.ws();
                if self.peek()? == b'}' {
                    self.p += 1;
                } else {
                    loop {
                        self.ws();
                        let k = self.string()?;
                        if k.escaped || n == MAX_KEYS || self.keys[base..].contains(&k.raw) {
                            return None; // escapes / many keys / duplicates: reference path
                        }
                        self.keys.push(k.raw);
                        n += 1;
                        write_str(out, k.s);
                        self.ws();
                        self.eat(b':')?;
                        if k.raw == b"type" {
                            self.ws();
                            if self.peek()? == b'"' {
                                *type_seen = true;
                            }
                        }
                        let mut dummy = false;
                        self.transcode(out, depth + 1, &mut dummy)?;
                        self.ws();
                        match self.peek()? {
                            b',' => self.p += 1,
                            b'}' => {
                                self.p += 1;
                                break;
                            }
                            _ => return None,
                        }
                    }
                }
                self.keys.truncate(base);
                patch_len(out, hdr, n, 0x80, 0xde, 0xdf);
            }
            b'[' => {
                self.p += 1;
                let hdr = out.len();
                out.push(0x90);
                let mut n = 0usize;
                self.ws();
                if self.peek()? == b']' {
                    self.p += 1;
                } else {
                    loop {
                        let mut dummy = false;
                        self.transcode(out, depth + 1, &mut dummy)?;
                        n += 1;
                        self.ws();
                        match self.peek()? {
                            b',' => self.p += 1,
                            b']' => {
                                self.p += 1;
                                break;
                            }
                            _ => return None,
                        }
                    }
                }
                patch_len(out, hdr, n, 0x90, 0xdc, 0xdd);
            }
            b't' => {
                self.lit(b"true")?;
                out.push(0xc3);
            }
            b'f' => {
                self.lit(b"false")?;
                out.push(0xc2);
            }
            b'n' => {
                self.lit(b"null")?;
                out.push(0xc0);
            }
            _ => {
                let (t, float) = self.number()?;
                if float {
                    return None; // floats: reference path (never in SDK actions)
                }
                let s = std::str::from_utf8(t).ok()?;
                if t[0] == b'-' {
                    write_int(out, s.parse::<i64>().ok()?);
                } else {
                    write_uint(out, s.parse::<u64>().ok()?);
                }
            }
        }
        Some(())
    }
}

/// Replace the 1-byte container header at `hdr` with the smallest header for
/// `n` elements (inserting bytes for 16-bit/32-bit lengths).
fn patch_len(out: &mut Vec<u8>, hdr: usize, n: usize, fix: u8, m16: u8, m32: u8) {
    if n < 16 {
        out[hdr] = fix | n as u8;
    } else if n < 0x1_0000 {
        let h = [m16, (n >> 8) as u8, n as u8];
        out.splice(hdr..hdr + 1, h);
    } else {
        let h = [m32, (n >> 24) as u8, (n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.splice(hdr..hdr + 1, h);
    }
}

/// JSON string unescape (surrogate pairs combined; lone surrogates rejected
/// like serde_json).
fn unescape(raw: &[u8]) -> O<String> {
    let s = std::str::from_utf8(raw).ok()?;
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let e = it.next()?;
        let ch = match e {
            '"' => '"',
            '\\' => '\\',
            '/' => '/',
            'b' => '\u{8}',
            'f' => '\u{c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'u' => {
                let hex4 = |it: &mut std::str::Chars<'_>| -> O<u32> {
                    let h: String = it.by_ref().take(4).collect();
                    if h.len() != 4 {
                        return None;
                    }
                    u32::from_str_radix(&h, 16).ok()
                };
                let u = hex4(&mut it)?;
                if (0xd800..0xdc00).contains(&u) {
                    if it.next()? != '\\' || it.next()? != 'u' {
                        return None;
                    }
                    let l = hex4(&mut it)?;
                    if !(0xdc00..0xe000).contains(&l) {
                        return None;
                    }
                    char::from_u32(0x10000 + ((u - 0xd800) << 10) + (l - 0xdc00))?
                } else if (0xdc00..0xe000).contains(&u) {
                    return None;
                } else {
                    char::from_u32(u)?
                }
            }
            _ => return None,
        };
        out.push(ch);
    }
    Some(out)
}

/// Fast path for [`crate::parse_exchange_request_bytes`]. `None` means "use
/// the reference path" (which also produces the error, if any).
pub fn parse_exchange_request_fast(body: &[u8]) -> Option<UserTx> {
    let mut j = J { b: body, p: 0, keys: Vec::with_capacity(32) };
    j.ws();
    j.eat(b'{')?;
    let mut keys: [&[u8]; MAX_KEYS] = [&[]; MAX_KEYS];
    let mut nkeys = 0;
    let mut action: Option<Vec<u8>> = None;
    let mut nonce = None;
    let mut sig = None;
    let mut vault: Option<Option<Address>> = None;
    let mut expires: Option<Option<u64>> = None;
    j.ws();
    if j.peek()? == b'}' {
        return None;
    }
    loop {
        j.ws();
        let k = j.string()?;
        if k.escaped || nkeys == MAX_KEYS || keys[..nkeys].contains(&k.raw) {
            return None;
        }
        keys[nkeys] = k.raw;
        nkeys += 1;
        j.ws();
        j.eat(b':')?;
        j.ws();
        match k.raw {
            b"action" => {
                if j.peek()? != b'{' {
                    return None;
                }
                let mut out = Vec::with_capacity(body.len());
                let mut type_seen = false;
                j.transcode(&mut out, 0, &mut type_seen)?;
                if !type_seen {
                    return None;
                }
                action = Some(out);
            }
            b"nonce" => nonce = Some(j.uint_token()?),
            b"signature" => {
                j.eat(b'{')?;
                let (mut r, mut s, mut v) = (None, None, None);
                let mut sk: [&[u8]; MAX_KEYS] = [&[]; MAX_KEYS];
                let mut n = 0;
                j.ws();
                if j.peek()? == b'}' {
                    return None;
                }
                loop {
                    j.ws();
                    let key = j.string()?;
                    if key.escaped || n == MAX_KEYS || sk[..n].contains(&key.raw) {
                        return None;
                    }
                    sk[n] = key.raw;
                    n += 1;
                    j.ws();
                    j.eat(b':')?;
                    j.ws();
                    match key.raw {
                        b"r" | b"s" => {
                            if j.peek()? != b'"' {
                                return None;
                            }
                            let t = j.string()?;
                            if t.escaped {
                                return None;
                            }
                            if key.raw == b"r" {
                                r = Some(t.s)
                            } else {
                                s = Some(t.s)
                            }
                        }
                        b"v" => v = Some(j.uint_token()?),
                        _ => j.skip(1)?,
                    }
                    j.ws();
                    match j.peek()? {
                        b',' => j.p += 1,
                        b'}' => {
                            j.p += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
                sig = Some(parse_signature_parts(r?, s?, v?).ok()?);
            }
            b"vaultAddress" => {
                vault = Some(match j.peek()? {
                    b'n' => {
                        j.lit(b"null")?;
                        None
                    }
                    b'"' => {
                        let t = j.string()?;
                        if t.escaped {
                            return None;
                        }
                        Some(parse_address(t.s).ok()?)
                    }
                    _ => return None,
                })
            }
            b"expiresAfter" => {
                expires = Some(match j.peek()? {
                    b'n' => {
                        j.lit(b"null")?;
                        None
                    }
                    _ => Some(j.uint_token()?),
                })
            }
            _ => j.skip(1)?,
        }
        j.ws();
        match j.peek()? {
            b',' => j.p += 1,
            b'}' => {
                j.p += 1;
                break;
            }
            _ => return None,
        }
    }
    j.ws();
    if j.p != body.len() {
        return None;
    }
    Some(UserTx {
        action_bytes: action?,
        nonce: nonce?,
        signature: sig?,
        vault_address: vault.flatten(),
        expires_after: expires.flatten(),
    })
}
