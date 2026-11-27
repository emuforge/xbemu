//! The crypto behind the kernel's Xc exports and the EEPROM: SHA-1,
//! HMAC-SHA1, RC4, DES / triple DES and modular exponentiation (FIPS 180-1,
//! RFC 2104, FIPS 46-3), written from the specifications. Plain functions
//! over host memory.
//!
//! The `to_bytes` / `from_bytes` forms are the layouts the reference keeps in
//! the contexts a title passes to the Xc exports (little-endian): the title
//! only hands them back, so they are opaque to it.

// ------------------------------------------------------------------ SHA-1

/// An incremental SHA-1.
#[derive(Clone)]
pub(crate) struct Sha1 {
    h: [u32; 5],
    /// Bytes in `buf`.
    n: u32,
    /// Total bytes.
    len: u64,
    buf: [u8; 64],
}

impl Sha1 {
    /// Bytes of the state as stored in a guest context (the XDK's context is
    /// 116 bytes).
    pub(crate) const BYTES: usize = 96;

    /// A hash with nothing in it.
    pub(crate) fn new() -> Self {
        Self {
            h: [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0],
            n: 0,
            len: 0,
            buf: [0; 64],
        }
    }

    fn block(h: &mut [u32; 5], p: &[u8; 64]) {
        let mut w = [0u32; 80];
        for (w, c) in w.iter_mut().zip(p.as_chunks::<4>().0) {
            *w = u32::from_be_bytes(*c);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = *h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..20 => ((b & c) | (!b & d), 0x5A82_7999),
                20..40 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..60 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6u32),
            };
            let t =
                a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (s, v) in h.iter_mut().zip([a, b, c, d, e]) {
            *s = s.wrapping_add(v);
        }
    }

    /// Hashes `data` after what was hashed before.
    pub(crate) fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        // a context a title filled itself may hold any count
        let mut n = (self.n as usize).min(63);
        while !data.is_empty() {
            let take = (64 - n).min(data.len());
            self.buf[n..n + take].copy_from_slice(&data[..take]);
            n += take;
            data = &data[take..];
            if n == 64 {
                Self::block(&mut self.h, &self.buf);
                n = 0;
            }
        }
        self.n = n as u32;
    }

    /// Pads and returns the digest. The state is left as the padding made it:
    /// start again with [`Sha1::new`].
    pub(crate) fn finalize(&mut self) -> [u8; 20] {
        let bits = self.len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.n != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut digest = [0u8; 20];
        for (o, h) in digest.as_chunks_mut::<4>().0.iter_mut().zip(self.h) {
            *o = h.to_be_bytes();
        }
        digest
    }

    /// The state as the guest context holds it: h at 0, the buffered count
    /// at 20, the total at 24, the buffer at 32.
    pub(crate) fn to_bytes(&self) -> [u8; Self::BYTES] {
        let mut b = [0u8; Self::BYTES];
        for (o, h) in b.as_chunks_mut::<4>().0.iter_mut().zip(self.h) {
            *o = h.to_le_bytes();
        }
        b[20..24].copy_from_slice(&self.n.to_le_bytes());
        b[24..32].copy_from_slice(&self.len.to_le_bytes());
        b[32..].copy_from_slice(&self.buf);
        b
    }

    /// The state out of a guest context ([`Sha1::to_bytes`]).
    pub(crate) fn from_bytes(b: &[u8; Self::BYTES]) -> Self {
        let mut s = Self::new();
        for (h, c) in s.h.iter_mut().zip(b.as_chunks::<4>().0) {
            *h = u32::from_le_bytes(*c);
        }
        s.n = u32::from_le_bytes([b[20], b[21], b[22], b[23]]);
        s.len = u64::from_le_bytes([b[24], b[25], b[26], b[27], b[28], b[29], b[30], b[31]]);
        s.buf.copy_from_slice(&b[32..]);
        s
    }
}

/// The SHA-1 of `data`.
pub(crate) fn sha1(data: &[u8]) -> [u8; 20] {
    let mut s = Sha1::new();
    s.update(data);
    s.finalize()
}

/// HMAC-SHA1 under `key` over `parts` one after another.
pub(crate) fn hmac_sha1(key: &[u8], parts: &[&[u8]]) -> [u8; 20] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..20].copy_from_slice(&sha1(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut s = Sha1::new();
    s.update(&k.map(|b| b ^ 0x36));
    for p in parts {
        s.update(p);
    }
    let inner = s.finalize();
    let mut s = Sha1::new();
    s.update(&k.map(|b| b ^ 0x5C));
    s.update(&inner);
    s.finalize()
}

// -------------------------------------------------------------------- RC4

/// An RC4 key stream.
#[derive(Clone)]
pub(crate) struct Rc4 {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    /// Bytes of the state as stored in a guest key struct (the XDK's size).
    pub(crate) const BYTES: usize = 258;

    /// The stream of `key`. An empty key leaves the identity permutation, as
    /// the reference does.
    pub(crate) fn new(key: &[u8]) -> Self {
        let mut s = [0u8; 256];
        for (i, v) in s.iter_mut().enumerate() {
            *v = i as u8;
        }
        if !key.is_empty() {
            let mut j = 0u8;
            for i in 0..256 {
                j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
                s.swap(i, usize::from(j));
            }
        }
        Self { s, i: 0, j: 0 }
    }

    /// Encrypts or decrypts `data` in place with the stream's next bytes.
    pub(crate) fn apply(&mut self, data: &mut [u8]) {
        let (mut i, mut j) = (self.i, self.j);
        for b in data {
            i = i.wrapping_add(1);
            j = j.wrapping_add(self.s[usize::from(i)]);
            self.s.swap(usize::from(i), usize::from(j));
            *b ^= self.s[usize::from(self.s[usize::from(i)].wrapping_add(self.s[usize::from(j)]))];
        }
        self.i = i;
        self.j = j;
    }

    /// The state as the guest key struct holds it: the permutation, i, j.
    pub(crate) fn to_bytes(&self) -> [u8; Self::BYTES] {
        let mut b = [0u8; Self::BYTES];
        b[..256].copy_from_slice(&self.s);
        b[256] = self.i;
        b[257] = self.j;
        b
    }

    /// The state out of a guest key struct ([`Rc4::to_bytes`]).
    pub(crate) fn from_bytes(b: &[u8; Self::BYTES]) -> Self {
        let mut s = [0u8; 256];
        s.copy_from_slice(&b[..256]);
        Self { s, i: b[256], j: b[257] }
    }
}

// -------------------------------------------------------------------- DES

const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6,
    64, 56, 48, 40, 32, 24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3, 61,
    53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];
const FP: [u8; 64] = [
    40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62, 30,
    37, 5, 45, 13, 53, 21, 61, 29, 36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27,
    34, 2, 42, 10, 50, 18, 58, 26, 33, 1, 41, 9, 49, 17, 57, 25,
];
const E: [u8; 48] = [
    32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9, 8, 9, 10, 11, 12, 13, 12, 13, 14, 15, 16, 17, 16, 17, 18,
    19, 20, 21, 20, 21, 22, 23, 24, 25, 24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
];
const P: [u8; 32] = [
    16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10, 2, 8, 24, 14, 32, 27, 3, 9, 19,
    13, 30, 6, 22, 11, 4, 25,
];
const PC1: [u8; 56] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60,
    52, 44, 36, 63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29,
    21, 13, 5, 28, 20, 12, 4,
];
const PC2: [u8; 48] = [
    14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2, 41, 52,
    31, 37, 47, 55, 30, 40, 51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];
const SHIFTS: [u8; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];
const SBOX: [[u8; 64]; 8] = [
    [
        14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12,
        11, 9, 5, 3, 8, 4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9,
        1, 7, 5, 11, 3, 14, 10, 0, 6, 13,
    ],
    [
        15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1,
        10, 6, 9, 11, 5, 0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1, 3, 15,
        4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
    ],
    [
        10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5,
        14, 12, 11, 15, 1, 13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6,
        9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12,
    ],
    [
        7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2,
        12, 1, 10, 14, 9, 10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10, 1,
        13, 8, 9, 4, 5, 11, 12, 7, 2, 14,
    ],
    [
        2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15,
        10, 3, 9, 8, 6, 4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14,
        2, 13, 6, 15, 0, 9, 10, 4, 5, 3,
    ],
    [
        12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13,
        14, 0, 11, 3, 8, 9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12, 9, 5,
        15, 10, 11, 14, 1, 7, 6, 0, 8, 13,
    ],
    [
        4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5,
        12, 2, 15, 8, 6, 1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4,
        10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
    ],
    [
        13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6,
        11, 0, 14, 9, 2, 7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10,
        8, 13, 15, 12, 9, 0, 3, 5, 6, 11,
    ],
];

/// Bit i of a table is bit i of the input counted from its most significant
/// bit (1-based).
fn permute(input: u64, in_bits: u32, table: &[u8]) -> u64 {
    table.iter().fold(0, |out, &t| (out << 1) | ((input >> (in_bits - u32::from(t))) & 1))
}

/// A DES key schedule: 16 48-bit round keys. Triple DES (EDE) uses three.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct Des {
    k: [u64; 16],
}

impl Des {
    /// Bytes of a schedule as stored in a guest key table.
    pub(crate) const BYTES: usize = 128;

    /// The schedule of `key`; the parity bits are not looked at.
    pub(crate) fn new(key: &[u8; 8]) -> Self {
        let cd = permute(u64::from_be_bytes(*key), 64, &PC1);
        let (mut c, mut d) = ((cd >> 28) as u32 & 0x0FFF_FFFF, cd as u32 & 0x0FFF_FFFF);
        let mut k = [0u64; 16];
        for (r, rk) in k.iter_mut().enumerate() {
            for _ in 0..SHIFTS[r] {
                c = ((c << 1) | (c >> 27)) & 0x0FFF_FFFF;
                d = ((d << 1) | (d >> 27)) & 0x0FFF_FFFF;
            }
            *rk = permute((u64::from(c) << 28) | u64::from(d), 56, &PC2);
        }
        Self { k }
    }

    fn f(r: u32, k: u64) -> u32 {
        let e = permute(u64::from(r), 32, &E) ^ k;
        let mut s = 0u32;
        for (i, sbox) in SBOX.iter().enumerate() {
            let six = (e >> (42 - 6 * i)) as usize & 0x3F;
            let (row, col) = (((six >> 4) & 2) | (six & 1), (six >> 1) & 0xF);
            s = (s << 4) | u32::from(sbox[row * 16 + col]);
        }
        permute(u64::from(s), 32, &P) as u32
    }

    /// Encrypts or decrypts one block.
    pub(crate) fn block(&self, input: &[u8; 8], encrypt: bool) -> [u8; 8] {
        let b = permute(u64::from_be_bytes(*input), 64, &IP);
        let (mut l, mut r) = ((b >> 32) as u32, b as u32);
        for i in 0..16 {
            let t = r;
            r = l ^ Self::f(r, self.k[if encrypt { i } else { 15 - i }] & 0xFFFF_FFFF_FFFF);
            l = t;
        }
        permute((u64::from(r) << 32) | u64::from(l), 64, &FP).to_be_bytes()
    }

    /// The schedule as the guest key table holds it: 16 little-endian
    /// 64-bit round keys.
    pub(crate) fn to_bytes(self) -> [u8; Self::BYTES] {
        let mut b = [0u8; Self::BYTES];
        for (o, k) in b.as_chunks_mut::<8>().0.iter_mut().zip(self.k) {
            *o = k.to_le_bytes();
        }
        b
    }

    /// The schedule out of a guest key table ([`Des::to_bytes`]).
    pub(crate) fn from_bytes(b: &[u8]) -> Self {
        let mut k = [0u64; 16];
        for (k, c) in k.iter_mut().zip(b.as_chunks::<8>().0) {
            *k = u64::from_le_bytes(*c);
        }
        Self { k }
    }
}

/// Triple DES, encrypt-decrypt-encrypt with three schedules.
pub(crate) fn des3_block(d: &[Des; 3], input: &[u8; 8], encrypt: bool) -> [u8; 8] {
    if encrypt {
        d[2].block(&d[1].block(&d[0].block(input, true), false), true)
    } else {
        d[0].block(&d[1].block(&d[2].block(input, false), true), false)
    }
}

/// Sets each byte's low bit for odd parity.
pub(crate) fn des_parity(key: &mut [u8]) {
    for b in key {
        let v = *b & 0xFE;
        *b = v | u8::from(v.count_ones() % 2 == 0);
    }
}

// ---------------------------------------------------------------- bignums

/// Whether the little-endian number `a` (one word longer than `m`, or as
/// long) is at least `m`.
fn ge(a: &[u32], m: &[u32]) -> bool {
    let n = m.len();
    if a.len() > n && a[n] != 0 {
        return true;
    }
    for i in (0..n).rev() {
        if a[i] != m[i] {
            return a[i] > m[i];
        }
    }
    true
}

/// a -= m (over the words of `a`; `m` is zero past its length).
fn sub(a: &mut [u32], m: &[u32]) {
    let mut borrow = 0u64;
    for (i, w) in a.iter_mut().enumerate() {
        let s = u64::from(m.get(i).copied().unwrap_or(0)) + borrow;
        borrow = u64::from(u64::from(*w) < s);
        *w = (u64::from(*w).wrapping_sub(s)) as u32;
    }
}

/// x mod m, in `m.len()` words: bitwise long division.
fn mod_reduce(x: &[u32], m: &[u32]) -> Vec<u32> {
    let n = m.len();
    let mut acc = vec![0u32; n + 1];
    for bit in (0..x.len() * 32).rev() {
        let mut carry = (x[bit / 32] >> (bit % 32)) & 1;
        for w in &mut acc {
            let next = *w >> 31;
            *w = (*w << 1) | carry;
            carry = next;
        }
        if ge(&acc, m) {
            sub(&mut acc, m);
        }
    }
    acc.truncate(n);
    acc
}

fn mul_mod(a: &[u32], b: &[u32], m: &[u32]) -> Vec<u32> {
    let n = m.len();
    let mut p = vec![0u32; 2 * n];
    for i in 0..n {
        let mut carry = 0u64;
        for j in 0..n {
            let t = u64::from(a[i]) * u64::from(b[j]) + u64::from(p[i + j]) + carry;
            p[i + j] = t as u32;
            carry = t >> 32;
        }
        p[i + n] = carry as u32;
    }
    mod_reduce(&p, m)
}

/// -m0^-1 mod 2^32 for an odd m0, by Newton's iteration (correct to 3, 6,
/// 12, 24, 48 bits).
fn mont_inv32(m0: u32) -> u32 {
    let mut x = m0;
    for _ in 0..5 {
        x = x.wrapping_mul(2u32.wrapping_sub(m0.wrapping_mul(x)));
    }
    x.wrapping_neg()
}

/// a * b / R mod m (a, b < m) into `w[..n]`; `w`: n + 2 words.
fn mont_mul(a: &[u32], b: &[u32], m: &[u32], mi: u32, w: &mut [u32]) {
    let n = m.len();
    w.fill(0);
    for &bi in b {
        let mut c = 0u64;
        for j in 0..n {
            let s = u64::from(w[j]) + u64::from(a[j]) * u64::from(bi) + c;
            w[j] = s as u32;
            c = s >> 32;
        }
        let mut s = u64::from(w[n]) + c;
        w[n] = s as u32;
        w[n + 1] = (s >> 32) as u32;
        let q = w[0].wrapping_mul(mi);
        s = u64::from(w[0]) + u64::from(q) * u64::from(m[0]);
        c = s >> 32;
        for j in 1..n {
            s = u64::from(w[j]) + u64::from(q) * u64::from(m[j]) + c;
            w[j - 1] = s as u32;
            c = s >> 32;
        }
        s = u64::from(w[n]) + c;
        w[n - 1] = s as u32;
        w[n] = w[n + 1].wrapping_add((s >> 32) as u32);
    }
    // w[0..=n] < 2m: once more m off if it is not under it
    if ge(&w[..=n], m) {
        sub(&mut w[..n], m);
    }
}

/// The exponent's bits from its highest set one down.
fn exponent_bits(c: &[u32]) -> impl Iterator<Item = bool> + '_ {
    let top = (0..c.len() * 32).rev().find(|&b| (c[b / 32] >> (b % 32)) & 1 != 0);
    top.into_iter().flat_map(move |t| (0..=t).rev().map(|b| (c[b / 32] >> (b % 32)) & 1 != 0))
}

/// An odd modulus (every RSA one): Montgomery products, n^2 word products
/// each where `mul_mod`'s reduction is a bit-by-bit division of the
/// double-length product (the disc's security-sector signature, a 2048-bit
/// private exponent, took 260 - 280 ms of the title's CPU at boot; 15 ms so).
fn modexp_mont(b: &[u32], c: &[u32], d: &[u32]) -> Vec<u32> {
    let n = d.len();
    let mi = mont_inv32(d[0]);
    let mut w = vec![0u32; n + 2];
    // base and 1 into the form by a reduction each
    let mut x = vec![0u32; 2 * n];
    x[n..].copy_from_slice(&mod_reduce(b, d));
    let bm = mod_reduce(&x, d); // b R
    x.fill(0);
    x[n] = 1;
    let mut rm = mod_reduce(&x, d); // R
    // then a square (and a product) a bit
    for bit in exponent_bits(c) {
        mont_mul(&rm, &rm, d, mi, &mut w);
        rm.copy_from_slice(&w[..n]);
        if bit {
            mont_mul(&rm, &bm, d, mi, &mut w);
            rm.copy_from_slice(&w[..n]);
        }
    }
    let mut one = vec![0u32; n];
    one[0] = 1;
    mont_mul(&rm, &one, d, mi, &mut w); // out of the form
    w.truncate(n);
    w
}

/// b^c mod d over little-endian 32-bit words, in `d.len()` words. `b` and `c`
/// are as long as `d`. `None` if `d` is zero or empty.
pub(crate) fn modexp(b: &[u32], c: &[u32], d: &[u32]) -> Option<Vec<u32>> {
    assert!(b.len() == d.len() && c.len() == d.len(), "modexp operands of one length");
    if d.iter().all(|&w| w == 0) {
        return None;
    }
    if d[0] & 1 != 0 {
        return Some(modexp_mont(b, c, d));
    }
    let base = mod_reduce(b, d);
    let mut one = vec![0u32; d.len()];
    one[0] = 1;
    let mut res = mod_reduce(&one, d); // 1 mod d
    for bit in exponent_bits(c) {
        res = mul_mod(&res, &res, d);
        if bit {
            res = mul_mod(&res, &base, d);
        }
    }
    Some(res)
}

/// Little-endian bytes (a multiple of 4) as little-endian words.
pub(crate) fn words_of(bytes: &[u8]) -> Vec<u32> {
    bytes.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect()
}

/// Little-endian words as little-endian bytes.
pub(crate) fn bytes_of(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        s.chunks(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect()
    }

    #[test]
    fn sha1_matches_the_fips_180_vectors() {
        assert_eq!(sha1(b"abc").to_vec(), hex("a9993e364706816aba3e25717850c26c9cd0d89d"));
        assert_eq!(sha1(b"").to_vec(), hex("da39a3ee5e6b4b0d3255bfef95601890afd80709"));
        assert_eq!(
            sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq").to_vec(),
            hex("84983e441c3bd26ebaae4aa1f95129e5e54670f1")
        );
        assert_eq!(
            sha1(&vec![b'a'; 1_000_000]).to_vec(),
            hex("34aa973cd4c4daa4f61eeb2bdbad27316534016f")
        );
    }

    #[test]
    fn sha1_in_pieces_and_through_its_guest_form_is_the_hash_of_the_whole() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        for cut in [0, 1, 55, 56, 63, 64, 65, 500, 999] {
            let mut s = Sha1::new();
            s.update(&data[..cut]);
            let mut s = Sha1::from_bytes(&s.to_bytes());
            s.update(&data[cut..]);
            assert_eq!(s.finalize(), sha1(&data), "cut at {cut}");
        }
    }

    #[test]
    fn hmac_sha1_matches_rfc_2202() {
        assert_eq!(
            hmac_sha1(&[0x0b; 20], &[b"Hi There"]).to_vec(),
            hex("b617318655057264e28bc0b6fb378c8ef146be00")
        );
        assert_eq!(
            hmac_sha1(b"Jefe", &[b"what do ya want ", b"for nothing?"]).to_vec(),
            hex("effcdf6ae5eb2fa2d27416d5f184df9c259a7c79")
        );
        assert_eq!(
            hmac_sha1(&[0xaa; 20], &[&[0xdd; 50]]).to_vec(),
            hex("125d7342b9ac11cd91a39af48aa17b4f63f175d3")
        );
        assert_eq!(
            hmac_sha1(&[0xaa; 80], &[b"Test Using Larger Than Block-Size Key - Hash Key First"])
                .to_vec(),
            hex("aa4ae5e15272d00e95705637ce8a3b55ed402112")
        );
    }

    #[test]
    fn rc4_matches_rfc_6229_and_decrypts_what_it_encrypted() {
        let mut r = Rc4::new(&hex("0102030405"));
        let mut s = [0u8; 32];
        r.apply(&mut s);
        assert_eq!(
            s.to_vec(),
            hex("b2396305f03dc027ccc3524a0a1118a8 6982944f18fc82d589c403a47a0d0919")
        );
        let mut r = Rc4::new(&hex("0102030405060708090a0b0c0d0e0f10"));
        let mut s = [0u8; 16];
        r.apply(&mut s);
        assert_eq!(s.to_vec(), hex("9ac7cc9a609d1ef7b2932899cde41b97"));

        let mut text = *b"Attack at dawn";
        Rc4::new(b"Secret").apply(&mut text);
        assert_eq!(text.to_vec(), hex("45a01f645fc35b383552544b9bf5"));
        // in two pieces through the guest form, the stream goes on
        let mut r = Rc4::new(b"Secret");
        r.apply(&mut text[..5]);
        let mut r = Rc4::from_bytes(&r.to_bytes());
        r.apply(&mut text[5..]);
        assert_eq!(&text, b"Attack at dawn");
    }

    #[test]
    fn des_matches_the_known_answers() {
        let d = Des::new(&hex("133457799BBCDFF1").try_into().unwrap());
        let c = d.block(&hex("0123456789ABCDEF").try_into().unwrap(), true);
        assert_eq!(c.to_vec(), hex("85E813540F0AB405"));
        assert_eq!(d.block(&c, false).to_vec(), hex("0123456789ABCDEF"));
        // NIST SP 800-17 variable plaintext, first entry
        let d = Des::new(&[1; 8]);
        assert_eq!(
            d.block(&hex("8000000000000000").try_into().unwrap(), true).to_vec(),
            hex("95F8A5E5DD31D900")
        );
        assert_eq!(Des::from_bytes(&d.to_bytes()), d);
    }

    #[test]
    fn triple_des_matches_sp_800_67_and_is_des_with_one_key_thrice() {
        let key = hex("0123456789ABCDEF 23456789ABCDEF01 456789ABCDEF0123");
        let d: [Des; 3] =
            std::array::from_fn(|i| Des::new(key[8 * i..8 * i + 8].try_into().unwrap()));
        // the document's plaintext is "The qufck brown fox jump"
        let c = des3_block(&d, b"The qufc", true);
        assert_eq!(c.to_vec(), hex("A826FD8CE53B855F"));
        assert_eq!(&des3_block(&d, &c, false), b"The qufc");

        let one = Des::new(b"\x13\x34\x57\x79\x9B\xBC\xDF\xF1");
        assert_eq!(des3_block(&[one; 3], b"12345678", true), one.block(b"12345678", true));
    }

    #[test]
    fn des_parity_makes_every_byte_odd() {
        let mut key: Vec<u8> = (0..=255).collect();
        des_parity(&mut key);
        for (i, b) in key.iter().enumerate() {
            assert_eq!(b.count_ones() % 2, 1);
            assert_eq!(usize::from(b & 0xFE), i & 0xFE);
        }
    }

    fn modexp_u64(b: u64, e: u64, m: u64) -> u64 {
        let (mut r, mut b, mut e) = (1u128 % u128::from(m), u128::from(b % m), e);
        while e != 0 {
            if e & 1 != 0 {
                r = r * b % u128::from(m);
            }
            b = b * b % u128::from(m);
            e >>= 1;
        }
        r as u64
    }

    fn w2(v: u64) -> [u32; 2] {
        [v as u32, (v >> 32) as u32]
    }

    #[test]
    fn modexp_agrees_with_wide_integer_arithmetic_for_odd_and_even_moduli() {
        assert_eq!(modexp(&[4], &[13], &[497]), Some(vec![445]));
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        for i in 0..200 {
            let mut next = || {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x
            };
            let (b, e, mut m) = (next(), next() >> (i % 64), next() >> (i % 40));
            if i % 2 == 0 {
                m |= 1;
            }
            let m = m.max(1);
            let r = modexp(&w2(b), &w2(e), &w2(m)).expect("a modulus that is not zero");
            assert_eq!(r, w2(modexp_u64(b, e, m)), "{b:x}^{e:x} mod {m:x}");
        }
        assert_eq!(modexp(&[5], &[0], &[1]), Some(vec![0]));
        assert_eq!(modexp(&[5, 0], &[3, 0], &[0, 0]), None);
        assert_eq!(modexp(&[], &[], &[]), None);
    }

    #[test]
    fn an_rsa_signature_comes_back_under_the_public_exponent() {
        // two Mersenne primes: p = 2^61 - 1, q = 2^31 - 1
        let (p, q) = ((1u128 << 61) - 1, (1u128 << 31) - 1);
        let n = p * q;
        let phi = (p - 1) * (q - 1);
        // d = e^-1 mod phi by the extended Euclid
        let e = 65537u128;
        let (mut r0, mut r1, mut t0, mut t1) = (phi as i128, e as i128, 0i128, 1i128);
        while r1 != 0 {
            let qt = r0 / r1;
            (r0, r1) = (r1, r0 - qt * r1);
            (t0, t1) = (t1, t0 - qt * t1);
        }
        let d = t0.rem_euclid(phi as i128) as u128;
        let w = |v: u128| [v as u32, (v >> 32) as u32, (v >> 64) as u32];
        let m = w(0x0123_4567_89AB_CDEF_0011);
        let sig = modexp(&m, &w(d), &w(n)).unwrap();
        assert_ne!(sig, m);
        assert_eq!(modexp(&sig, &w(e), &w(n)).unwrap(), m);
    }

    #[test]
    fn words_and_bytes_convert_both_ways() {
        let b = hex("0102030405060708");
        assert_eq!(words_of(&b), vec![0x0403_0201, 0x0807_0605]);
        assert_eq!(bytes_of(&words_of(&b)), b);
    }
}
