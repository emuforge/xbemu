//! The kernel's crypto exports (Xc*).
//!
//! The contexts the guest passes are opaque buffers sized by the XDK: SHA 116
//! bytes, RC4 258, DES key table 128 (triple DES 384); each holds the host
//! state in the layout [`crate::xc_crypto`] gives it.
//!
//! Public keys are BSAFE blobs: magic "RSA1", keylen 0x04 (modulus bytes with
//! padding), bitlen 0x08, datalen 0x0C, pubexp 0x10, modulus at 0x14
//! (little-endian). Numbers in XcModExp and signatures are little-endian too.
//!
//! Not here: XcPKDecPrivate (no title has a private key to pass it).

use crate::gm::KResult;
use crate::kernel::{Args, Kernel};
use crate::platform::Platform;
use crate::xc_crypto::{
    Des, Rc4, Sha1, bytes_of, des_parity, des3_block, hmac_sha1, modexp, words_of,
};

const XC_DES3: u32 = 1;

/// "RSA1".
const RSA1: u32 = 0x3141_5352;
const PK_KEYLEN: u32 = 0x04;
const PK_BITLEN: u32 = 0x08;
const PK_PUBEXP: u32 = 0x10;
const PK_MODULUS: u32 = 0x14;

/// Bytes of XePublicKeyData.
pub(crate) const PUBLIC_KEY_LEN: usize = 0x11C;

/// The longest number XcModExp takes, in 32-bit words (a 32768-bit one; the
/// console's are 2048).
const MODEXP_MAX_WORDS: u32 = 1024;

/// This kernel's own RSA-2048 key for the discs' security sectors
/// ([`security_sector`]): the modulus is the kernel's XePublicKeyData, the
/// private exponent signs the sectors the DVD drive hands out. Not the
/// console's key (a disc image has no security sector of its own, and the
/// console's private key signs none here): a title checks the sector against
/// the kernel's public key, and that key is this one. Little-endian 32-bit
/// words.
const SS_N: [u32; 64] = [
    0x352e_bcc9,
    0xaf70_182b,
    0x5989_3784,
    0x40be_aca0,
    0xceaa_bcc8,
    0x6963_c729,
    0xc09c_f927,
    0xefad_28f5,
    0xfb46_312e,
    0x2bab_8a6d,
    0x5c1c_1fd9,
    0xd38d_accc,
    0x6750_3a9f,
    0x52d4_712e,
    0x2f98_c310,
    0xf068_282a,
    0x7674_1311,
    0x852e_4d2e,
    0x1518_727e,
    0x6774_4e1e,
    0x1ee4_edc1,
    0x7feb_20cc,
    0xc756_a519,
    0x4928_7a87,
    0x750e_53b3,
    0x8626_ca07,
    0x5355_70d1,
    0xa4c8_98a5,
    0x68eb_cda4,
    0x428e_9c10,
    0xa3c9_1713,
    0xa420_bc9e,
    0xd848_f688,
    0xbd36_bcdb,
    0x8439_26e2,
    0x3640_d71c,
    0x2491_37ac,
    0xf872_00c2,
    0x90c3_b8ae,
    0xce97_8862,
    0x2c7a_7c83,
    0xe527_7345,
    0x6450_db32,
    0xb8c2_6e57,
    0xf397_6461,
    0xbc07_a7c3,
    0x93dc_9f46,
    0x7740_f339,
    0x654c_6785,
    0xbbc7_f334,
    0xabaa_31a2,
    0x1d48_360c,
    0xd633_d371,
    0x5e32_8541,
    0xd139_1f1c,
    0x91e4_50e4,
    0xd956_3d65,
    0x173c_db3f,
    0xdf2f_4cf6,
    0x1c69_782f,
    0xcb18_80a5,
    0x6e6c_3f76,
    0x38d6_86f4,
    0xaa2b_8f6b,
];
const SS_D: [u32; 64] = [
    0x47de_2a01,
    0x2baa_7486,
    0xa2cf_4836,
    0x26fb_fa5d,
    0x84e0_cf0c,
    0x1012_d10c,
    0xce73_11f2,
    0x1b74_849e,
    0x93b3_4ceb,
    0x11f6_21b5,
    0xf16e_3859,
    0xf9e0_7c4d,
    0xbe0f_0ea8,
    0x9ea2_2428,
    0x9e4a_ace7,
    0xd576_4a84,
    0xc08c_192a,
    0x745e_58a5,
    0xe2ff_1564,
    0x26ef_ecc6,
    0x1094_8022,
    0xd62d_ff4e,
    0xf076_117a,
    0x39cd_01c1,
    0xb5db_1dba,
    0x6eeb_0efd,
    0xf2a0_781a,
    0x6be2_3374,
    0x4a33_e204,
    0xf9a8_a76a,
    0x3529_8733,
    0x72a6_3c4e,
    0x80d2_8c7e,
    0xda49_4ec2,
    0x9bb8_f17f,
    0x38c7_fba9,
    0x9b2e_60c2,
    0x0296_46b6,
    0x4974_ee70,
    0x20c6_152b,
    0xed59_2032,
    0xc660_3090,
    0xed40_5a10,
    0xc615_74e7,
    0x1512_c155,
    0x1a66_8eb2,
    0xb817_4572,
    0x881c_b9bf,
    0x153b_ee29,
    0x6e0e_48e9,
    0x3c69_3975,
    0x19b6_31bf,
    0x97c4_8b36,
    0x228d_e376,
    0x19d7_1c44,
    0x24ef_5bd8,
    0xd6ab_361c,
    0xd74b_7b01,
    0x9abc_ac0a,
    0xc9a7_3336,
    0xc23b_29e5,
    0x3d26_7446,
    0x557d_b1e1,
    0x0286_c509,
];

fn sha_load(k: &Kernel, va: u32) -> KResult<Sha1> {
    let mut b = [0u8; Sha1::BYTES];
    k.gm.read(va, &mut b)?;
    Ok(Sha1::from_bytes(&b))
}

/// XcSHAInit(pbSHAContext)
fn xc_sha_init(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    k.gm.write(a[0], &Sha1::new().to_bytes())?;
    Ok(0)
}

/// XcSHAUpdate(pbSHAContext, pbInput, dwInputLength)
fn xc_sha_update(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let mut s = sha_load(k, a[0])?;
    s.update(&k.gm.read_vec(a[1], a[2])?);
    k.gm.write(a[0], &s.to_bytes())?;
    Ok(0)
}

/// XcSHAFinal(pbSHAContext, pbDigest)
fn xc_sha_final(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let mut s = sha_load(k, a[0])?;
    let digest = s.finalize();
    // the digest first: the call runs again, from the context as it was, if
    // its page is not in
    k.gm.write(a[1], &digest)?;
    k.gm.write(a[0], &s.to_bytes())?;
    Ok(0)
}

/// XcHMAC(pbKey, dwKeyLength, pbInput, dwInputLength, pbInput2, dwInputLength2, pbDigest)
fn xc_hmac(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let key = k.gm.read_vec(a[0], a[1])?;
    let in1 = if a[2] != 0 { k.gm.read_vec(a[2], a[3])? } else { Vec::new() };
    let in2 = if a[4] != 0 { k.gm.read_vec(a[4], a[5])? } else { Vec::new() };
    k.gm.write(a[6], &hmac_sha1(&key, &[&in1, &in2]))?;
    Ok(0)
}

/// XcRC4Key(pbKeyStruct, dwKeyLength, pbKey)
fn xc_rc4_key(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let key = k.gm.read_vec(a[2], a[1])?;
    k.gm.write(a[0], &Rc4::new(&key).to_bytes())?;
    Ok(0)
}

/// XcRC4Crypt(pbKeyStruct, dwInputLength, pbInput)
fn xc_rc4_crypt(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let mut b = [0u8; Rc4::BYTES];
    k.gm.read(a[0], &mut b)?;
    let mut r = Rc4::from_bytes(&b);
    let mut data = k.gm.read_vec(a[2], a[1])?;
    r.apply(&mut data);
    k.gm.write(a[2], &data)?;
    k.gm.write(a[0], &r.to_bytes())?;
    Ok(0)
}

fn keys(cipher: u32) -> usize {
    if cipher == XC_DES3 { 3 } else { 1 }
}

/// The key table at `va`: one schedule, three for triple DES (the others
/// unused).
fn key_table(k: &Kernel, cipher: u32, va: u32) -> KResult<[Des; 3]> {
    let b = k.gm.read_vec(va, (keys(cipher) * Des::BYTES) as u32)?;
    let mut d = [Des::default(); 3];
    for (d, c) in d.iter_mut().zip(b.as_chunks::<{ Des::BYTES }>().0) {
        *d = Des::from_bytes(c);
    }
    Ok(d)
}

fn block(cipher: u32, d: &[Des; 3], input: &[u8; 8], encrypt: bool) -> [u8; 8] {
    if cipher == XC_DES3 { des3_block(d, input, encrypt) } else { d[0].block(input, encrypt) }
}

fn read8(k: &Kernel, va: u32) -> KResult<[u8; 8]> {
    let mut b = [0u8; 8];
    k.gm.read(va, &mut b)?;
    Ok(b)
}

/// XcKeyTable(dwCipher, pbKeyTable, pbKey): dwCipher 0 is DES, 1 triple DES.
fn xc_key_table(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let n = keys(a[0]);
    let key = k.gm.read_vec(a[2], 8 * n as u32)?;
    let mut table = Vec::with_capacity(n * Des::BYTES);
    for c in key.as_chunks::<8>().0 {
        table.extend_from_slice(&Des::new(c).to_bytes());
    }
    k.gm.write(a[1], &table)?;
    Ok(0)
}

/// XcBlockCrypt(dwCipher, pbOutput, pbInput, pbKeyTable, dwOp): dwOp 1 encrypts.
fn xc_block_crypt(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let d = key_table(k, a[0], a[3])?;
    let input = read8(k, a[2])?;
    k.gm.write(a[1], &block(a[0], &d, &input, a[4] != 0))?;
    Ok(0)
}

/// XcBlockCryptCBC(dwCipher, dwInputLength, pbOutput, pbInput, pbKeyTable, dwOp, pbFeedback)
fn xc_block_crypt_cbc(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let d = key_table(k, a[0], a[4])?;
    let len = a[1] & !7;
    // every page first: the feedback must not move on for blocks a rerun of
    // the call would do again
    k.gm.read_vec(a[3], len)?;
    k.gm.read_vec(a[2], len)?;
    read8(k, a[6])?;
    // a block at a time, in the reference's order: the buffers may be one
    for off in (0..len).step_by(8) {
        let input = read8(k, a[3].wrapping_add(off))?;
        let mut fb = read8(k, a[6])?;
        let out = a[2].wrapping_add(off);
        if a[5] != 0 {
            let mut x = input;
            for (x, f) in x.iter_mut().zip(fb) {
                *x ^= f;
            }
            fb = block(a[0], &d, &x, true);
            k.gm.write(out, &fb)?;
        } else {
            let mut x = block(a[0], &d, &input, false);
            for (x, f) in x.iter_mut().zip(fb) {
                *x ^= f;
            }
            k.gm.write(out, &x)?;
            fb = input;
        }
        k.gm.write(a[6], &fb)?;
    }
    Ok(0)
}

/// XcDESKeyParity(pbKey, dwKeyLength)
fn xc_des_key_parity(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let mut key = k.gm.read_vec(a[0], a[1])?;
    des_parity(&mut key);
    k.gm.write(a[0], &key)?;
    Ok(0)
}

/// XcModExp(pA, pB, pC, pD, dwN): A = B^C mod D, over dwN 32-bit words.
/// False if D is zero.
fn xc_mod_exp(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let n = a[4];
    if n > MODEXP_MAX_WORDS {
        log::warn!(target: "kernel", "xc: XcModExp of {n} words refused");
        return Ok(0);
    }
    let b = words_of(&k.gm.read_vec(a[1], n * 4)?);
    let c = words_of(&k.gm.read_vec(a[2], n * 4)?);
    let d = words_of(&k.gm.read_vec(a[3], n * 4)?);
    let Some(r) = modexp(&b, &c, &d) else { return Ok(0) };
    k.gm.write(a[0], &bytes_of(&r))?;
    Ok(1)
}

/// XcPKGetKeyLen(pbPubKey)
fn xc_pk_get_key_len(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    Ok(u64::from(k.gm.r32(a[0] + PK_KEYLEN)?))
}

/// The public-key operation on a BSAFE key: in^e mod n, keylen bytes, of the
/// bitlen / 8 bytes at `input`. `None`: not a key.
fn pk_public(k: &Kernel, key: u32, input: u32) -> KResult<Option<Vec<u8>>> {
    let (keylen, bitlen) = (k.gm.r32(key + PK_KEYLEN)?, k.gm.r32(key + PK_BITLEN)?);
    if k.gm.r32(key)? != RSA1 || keylen < bitlen / 8 || keylen > 1024 || keylen % 4 != 0 {
        return Ok(None);
    }
    let mut x = k.gm.read_vec(input, bitlen / 8)?;
    x.resize(keylen as usize, 0);
    let mut e = vec![0u32; keylen as usize / 4];
    if let Some(e0) = e.first_mut() {
        *e0 = k.gm.r32(key + PK_PUBEXP)?;
    }
    let n = words_of(&k.gm.read_vec(key + PK_MODULUS, keylen)?);
    Ok(modexp(&words_of(&x), &e, &n).map(|r| bytes_of(&r)))
}

/// XcPKEncPublic(pbPubKey, pbInput, pbOutput)
fn xc_pk_enc_public(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let Some(out) = pk_public(k, a[0], a[1])? else { return Ok(0) };
    k.gm.write(a[2], &out)?;
    Ok(1)
}

/// XcVerifyPKCS1Signature(pbSig, pbPubKey, pbDigest): the signature decrypts
/// to a PKCS #1 type 1 block holding the SHA-1 digest. The block's byte order
/// isn't pinned down yet, so both orders are accepted and the match is
/// logged.
fn xc_verify_pkcs1_signature(k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    let mut digest = [0u8; 20];
    k.gm.read(a[2], &mut digest)?;
    let Some(block_le) = pk_public(k, a[1], a[0])? else {
        log::info!(target: "kernel", "xc: PKCS1 verify: bad public key");
        return Ok(0);
    };
    let len = (k.gm.r32(a[1] + PK_BITLEN)? / 8) as usize;
    let be: Vec<u8> = block_le[..len].iter().rev().copied().collect();
    let padded = len > 23 && be[0] == 0 && be[1] == 1;
    let fwd = len >= 20 && be[len - 20..] == digest;
    let rev = len >= 20 && block_le[..20].iter().eq(digest.iter().rev());
    log::info!(target: "kernel", "xc: PKCS1 verify: {} (block type {}, digest {})",
        if fwd || rev { "ok" } else { "FAILED" },
        if padded { "1" } else { "?" },
        if fwd { "big-endian" } else if rev { "little-endian" } else { "no match" });
    Ok(u64::from(fwd || rev))
}

/// The kernel's public key (XePublicKeyData, 0x11C bytes): a BSAFE key,
/// "RSA1", its length with padding (0x108), 2048 bits, 255 bytes of data,
/// the exponent, the modulus. This kernel's own.
pub(crate) fn public_key() -> [u8; PUBLIC_KEY_LEN] {
    let mut p = [0u8; PUBLIC_KEY_LEN];
    for (i, v) in [RSA1, 0x108, 2048, 255, 65537].into_iter().enumerate() {
        p[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
    }
    p[PK_MODULUS as usize..PK_MODULUS as usize + 256].copy_from_slice(&bytes_of(&SS_N));
    p
}

/// Where the running XBE's header points to its certificate, and the time
/// stamp in the certificate.
const XBE_CERTIFICATE_PTR: u32 = 0x0001_0118;
const CERT_TIMESTAMP: u32 = 4;

/// The disc's security sector as READ DVD STRUCTURE returns it (address
/// FF02FDFF, layer FE: 0x664 bytes, a 4-byte header first), into all of
/// `out`. A disc image has none (the Xbox's are on the disc's lead-in,
/// outside the image), so the drive makes one that passes the titles' own
/// check, XAPI's for a title whose certificate's security flags ask for it
/// (Crimson Skies: High Road to Revenge, which went back to the dashboard on
/// zeros): the certificate's time stamp at +0x4AB, the SHA-1 of the length
/// (0x4CB) and the 0x4CB bytes from +4 at +0x4CF, and the PKCS #1 signature
/// of that digest under the kernel's public key at +0x4E3. False, and `out`
/// untouched, when `out` is too short for it or the title's certificate
/// cannot be read.
pub(crate) fn security_sector(k: &Kernel, out: &mut [u8]) -> bool {
    let n = out.len();
    if n < 0x4E3 + 256 {
        return false;
    }
    let stamp = k.gm.r32(XBE_CERTIFICATE_PTR).and_then(|cert| k.gm.r32(cert + CERT_TIMESTAMP));
    let Ok(stamp) = stamp else {
        log::warn!(target: "kernel", "xc: security sector: no certificate to take the time stamp from");
        return false;
    };
    out.fill(0);
    // the structure's length past itself, big-endian
    out[..2].copy_from_slice(&((n - 2) as u16).to_be_bytes());
    out[0x4AB..0x4AF].copy_from_slice(&stamp.to_le_bytes());
    let mut s = Sha1::new();
    s.update(&0x4CBu32.to_le_bytes());
    s.update(&out[4..4 + 0x4CB]);
    let digest = s.finalize();
    out[0x4CF..0x4E3].copy_from_slice(&digest);
    // 00 01 FF..FF 00 digest, read as the little-endian number it is
    let mut m = [0xFFu8; 256];
    m[..20].iter_mut().zip(digest.iter().rev()).for_each(|(m, d)| *m = *d);
    m[20] = 0;
    m[254] = 1;
    m[255] = 0;
    let Some(sig) = modexp(&words_of(&m), &SS_D, &SS_N) else { return false };
    out[0x4E3..0x4E3 + 256].copy_from_slice(&bytes_of(&sig));
    true
}

/// XcCryptService(dwOp, pArgs): no replaceable crypto vector.
fn xc_crypt_service(_k: &mut Kernel, _p: &mut dyn Platform, a: &Args) -> KResult<u64> {
    log::info!(target: "kernel", "xc: XcCryptService({}) not implemented", a[0]);
    Ok(0)
}

/// XcUpdateCrypto(pNewVector, pROMVector): no replaceable crypto vector.
fn xc_update_crypto(_k: &mut Kernel, _p: &mut dyn Platform, _a: &Args) -> KResult<u64> {
    log::info!(target: "kernel", "xc: XcUpdateCrypto ignored");
    Ok(0)
}

pub(crate) fn register(k: &mut Kernel) {
    k.imp("XcSHAInit", xc_sha_init);
    k.imp("XcSHAUpdate", xc_sha_update);
    k.imp("XcSHAFinal", xc_sha_final);
    k.imp("XcHMAC", xc_hmac);
    k.imp("XcRC4Key", xc_rc4_key);
    k.imp("XcRC4Crypt", xc_rc4_crypt);
    k.imp("XcKeyTable", xc_key_table);
    k.imp("XcBlockCrypt", xc_block_crypt);
    k.imp("XcBlockCryptCBC", xc_block_crypt_cbc);
    k.imp("XcDESKeyParity", xc_des_key_parity);
    k.imp("XcModExp", xc_mod_exp);
    k.imp("XcPKGetKeyLen", xc_pk_get_key_len);
    k.imp("XcPKEncPublic", xc_pk_enc_public);
    k.imp("XcVerifyPKCS1Signature", xc_verify_pkcs1_signature);
    k.imp("XcCryptService", xc_crypt_service);
    k.imp("XcUpdateCrypto", xc_update_crypto);
    k.gm.kwrite(k.data_addr("XePublicKeyData"), &public_key());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{self, TestKernel};
    use crate::xc_crypto::sha1;

    fn put(t: &mut TestKernel, data: &[u8]) -> u32 {
        let va = t.alloc(data.len() as u32);
        t.k.gm.write(va, data).unwrap();
        va
    }

    fn get(t: &TestKernel, va: u32, len: u32) -> Vec<u8> {
        t.k.gm.read_vec(va, len).unwrap()
    }

    #[test]
    fn the_sha_exports_hash_in_pieces_what_sha1_hashes_whole() {
        let mut t = testing::kernel();
        let data: Vec<u8> = (0..300u32).map(|i| (i * 13) as u8).collect();
        let (ctx, buf, out) = (t.alloc(116), put(&mut t, &data), t.alloc(20));
        t.call("XcSHAInit", &[ctx]);
        t.call("XcSHAUpdate", &[ctx, buf, 100]);
        t.call("XcSHAUpdate", &[ctx, buf + 100, 200]);
        t.call("XcSHAFinal", &[ctx, out]);
        assert!(t.returned());
        assert_eq!(get(&t, out, 20), sha1(&data));
    }

    #[test]
    fn xc_hmac_signs_its_two_inputs_as_one_and_takes_a_missing_one_as_empty() {
        let mut t = testing::kernel();
        let (key, text, out) =
            (put(&mut t, b"Jefe"), put(&mut t, b"what do ya want for nothing?"), t.alloc(20));
        t.call("XcHMAC", &[key, 4, text, 16, text + 16, 12, out]);
        let want = hmac_sha1(b"Jefe", &[b"what do ya want for nothing?"]);
        assert_eq!(get(&t, out, 20), want);
        t.call("XcHMAC", &[key, 4, 0, 99, text, 28, out]);
        assert_eq!(get(&t, out, 20), want);
    }

    #[test]
    fn the_rc4_exports_keep_the_stream_in_the_guest_key_struct() {
        let mut t = testing::kernel();
        let (ks, key, text) =
            (t.alloc(258), put(&mut t, b"Secret"), put(&mut t, b"Attack at dawn"));
        t.call("XcRC4Key", &[ks, 6, key]);
        t.call("XcRC4Crypt", &[ks, 6, text]);
        t.call("XcRC4Crypt", &[ks, 8, text + 6]);
        let mut want = *b"Attack at dawn";
        Rc4::new(b"Secret").apply(&mut want);
        assert_eq!(get(&t, text, 14), want);
        t.call("XcRC4Key", &[ks, 6, key]);
        t.call("XcRC4Crypt", &[ks, 14, text]);
        assert_eq!(get(&t, text, 14), b"Attack at dawn");
    }

    #[test]
    fn xc_block_crypt_is_des_or_triple_des_by_the_cipher() {
        let mut t = testing::kernel();
        let key = put(
            &mut t,
            &[
                0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD,
                0xEF, 0x01, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23,
            ],
        );
        let (table, text, out, back) =
            (t.alloc(384), put(&mut t, b"The qufc"), t.alloc(8), t.alloc(8));
        t.call("XcKeyTable", &[1, table, key]);
        t.call("XcBlockCrypt", &[1, out, text, table, 1]);
        assert_eq!(get(&t, out, 8), [0xA8, 0x26, 0xFD, 0x8C, 0xE5, 0x3B, 0x85, 0x5F]);
        t.call("XcBlockCrypt", &[1, back, out, table, 0]);
        assert_eq!(get(&t, back, 8), b"The qufc");

        t.call("XcKeyTable", &[0, table, key]);
        t.call("XcBlockCrypt", &[0, out, text, table, 1]);
        let d = Des::new(&[0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF]);
        assert_eq!(get(&t, out, 8), d.block(b"The qufc", true));
    }

    #[test]
    fn xc_block_crypt_cbc_chains_blocks_and_decrypts_in_place() {
        let mut t = testing::kernel();
        let key = put(&mut t, b"8bytekey");
        let plain = b"sixteen bytes of text, and more!";
        let (table, text, out, fb) = (t.alloc(128), put(&mut t, plain), t.alloc(32), t.alloc(8));
        let iv = *b"\x01\x02\x03\x04\x05\x06\x07\x08";
        t.call("XcKeyTable", &[0, table, key]);
        t.k.gm.write(fb, &iv).unwrap();
        t.call("XcBlockCryptCBC", &[0, 32, out, text, table, 1, fb]);
        let cipher = get(&t, out, 32);
        // the first block is DES of plain ^ iv, the feedback the last block
        let d = Des::new(b"8bytekey");
        let mut x: [u8; 8] = plain[..8].try_into().unwrap();
        x.iter_mut().zip(iv).for_each(|(x, i)| *x ^= i);
        assert_eq!(cipher[..8], d.block(&x, true));
        assert_eq!(get(&t, fb, 8), cipher[24..]);
        assert_ne!(cipher[8..16], d.block(plain[8..16].try_into().unwrap(), true));
        // in two calls, in place: the feedback carries the chain
        t.k.gm.write(fb, &iv).unwrap();
        t.call("XcBlockCryptCBC", &[0, 16, out, out, table, 0, fb]);
        t.call("XcBlockCryptCBC", &[0, 16, out + 16, out + 16, table, 0, fb]);
        assert_eq!(get(&t, out, 32), plain);
    }

    #[test]
    fn xc_des_key_parity_sets_odd_parity_in_place() {
        let mut t = testing::kernel();
        let key = put(&mut t, &[0x00, 0x01, 0x02, 0xFF, 0x80]);
        t.call("XcDESKeyParity", &[key, 4]);
        assert_eq!(get(&t, key, 5), [0x01, 0x01, 0x02, 0xFE, 0x80]);
    }

    #[test]
    fn xc_mod_exp_computes_over_guest_words_and_refuses_a_zero_modulus() {
        let mut t = testing::kernel();
        let (a, b, c, d) = (
            t.alloc(8),
            put(&mut t, &[4, 0, 0, 0, 0, 0, 0, 0]),
            put(&mut t, &[13, 0, 0, 0, 0, 0, 0, 0]),
            put(&mut t, &[0xF1, 1, 0, 0, 0, 0, 0, 0]),
        );
        assert_eq!(t.call32("XcModExp", &[a, b, c, d, 2]), 1);
        assert_eq!(get(&t, a, 8), [0xBD, 1, 0, 0, 0, 0, 0, 0]); // 4^13 mod 497 = 445
        let zero = t.alloc(8);
        assert_eq!(t.call32("XcModExp", &[a, b, c, zero, 2]), 0);
    }

    /// A title-shaped XBE header: the certificate pointer and a time stamp.
    fn with_certificate(t: &mut TestKernel, stamp: u32) {
        assert!(t.k.gm.alloc_at(0x10000, 0x1000));
        t.k.gm.w32(XBE_CERTIFICATE_PTR, 0x10180).unwrap();
        t.k.gm.w32(0x10180 + CERT_TIMESTAMP, stamp).unwrap();
    }

    #[test]
    fn the_security_sector_passes_the_check_a_title_makes_with_the_kernel_exports() {
        let mut t = testing::kernel();
        with_certificate(&mut t, 0x3F12_3456);
        let mut ss = vec![0xEEu8; 0x664];
        assert!(security_sector(&t.k, &mut ss));
        assert_eq!(ss[..2], [0x06, 0x62]);
        assert_eq!(ss[0x4AB..0x4AF], 0x3F12_3456u32.to_le_bytes());

        // XAPI's check: SHA-1 of the length and the bytes, then the
        // signature against XePublicKeyData
        let (buf, ctx, len, digest) =
            (put(&mut t, &ss), t.alloc(116), put(&mut t, &0x4CBu32.to_le_bytes()), t.alloc(20));
        t.call("XcSHAInit", &[ctx]);
        t.call("XcSHAUpdate", &[ctx, len, 4]);
        t.call("XcSHAUpdate", &[ctx, buf + 4, 0x4CB]);
        t.call("XcSHAFinal", &[ctx, digest]);
        assert_eq!(get(&t, digest, 20), ss[0x4CF..0x4E3]);
        let key = t.k.data_addr("XePublicKeyData");
        assert_eq!(get(&t, key, PUBLIC_KEY_LEN as u32), public_key());
        assert_eq!(t.call32("XcPKGetKeyLen", &[key]), 0x108);
        assert_eq!(t.call32("XcVerifyPKCS1Signature", &[buf + 0x4E3, key, digest]), 1);

        // the signature opens to the PKCS #1 block
        let out = t.alloc(0x108);
        assert_eq!(t.call32("XcPKEncPublic", &[key, buf + 0x4E3, out]), 1);
        let block = get(&t, out, 0x108);
        assert_eq!(block[..20].iter().rev().copied().collect::<Vec<_>>(), ss[0x4CF..0x4E3]);
        assert_eq!(block[20], 0);
        assert!(block[21..254].iter().all(|&b| b == 0xFF));
        assert_eq!(block[254..], [1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

        // another digest does not verify
        t.k.gm.w8(digest, ss[0x4CF] ^ 1).unwrap();
        assert_eq!(t.call32("XcVerifyPKCS1Signature", &[buf + 0x4E3, key, digest]), 0);
    }

    #[test]
    fn the_security_sector_needs_room_and_a_certificate() {
        let mut t = testing::kernel();
        let mut ss = vec![0xEEu8; 0x664];
        assert!(!security_sector(&t.k, &mut ss), "no XBE header is mapped");
        assert!(ss.iter().all(|&b| b == 0xEE));
        with_certificate(&mut t, 1);
        assert!(!security_sector(&t.k, &mut ss[..0x5E2]));
        assert!(security_sector(&t.k, &mut ss[..0x5E3]));
    }

    #[test]
    fn a_key_that_is_not_rsa1_is_refused() {
        let mut t = testing::kernel();
        let mut key = public_key();
        key[0] = b'X';
        let (key, sig, out) = (put(&mut t, &key), t.alloc(256), t.alloc(0x108));
        assert_eq!(t.call32("XcPKEncPublic", &[key, sig, out]), 0);
        assert_eq!(t.call32("XcVerifyPKCS1Signature", &[sig, key, out]), 0);
    }

    #[test]
    fn a_context_on_a_missing_page_is_the_titles_fault_and_changes_nothing() {
        let mut t = testing::kernel();
        let ctx = t.alloc(116);
        t.call("XcSHAInit", &[ctx]);
        let before = get(&t, ctx, 116);
        let page = t.alloc(1);
        // the input runs off its page into the unmapped one after it
        t.call("XcSHAUpdate", &[ctx, page + 0xFF0, 0x20]);
        assert!(!t.returned());
        assert_eq!(get(&t, ctx, 116), before);
    }
}
