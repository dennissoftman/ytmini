//! Checks yt-dlp's signed checksum file (`SHA2-256SUMS.sig`) against the
//! yt-dlp signing key built into the app.
//!
//! Without this check, a download is compared only with hashes from the same
//! release, so anyone who can replace the release files can replace both.
//! The key below is pinned: a release signed with any other key is rejected,
//! and the yt-dlp already installed stays in use.
//!
//! This handles only what yt-dlp publishes: a detached OpenPGP v4 signature
//! over binary data, made with an RSA key and SHA-256 or SHA-512.

use anyhow::{bail, ensure, Context, Result};
use rsa::{BoxedUint, Pkcs1v15Sign, PublicKey};
use sha2::{Digest, Sha256, Sha512};

/// yt-dlp's release signing key ("Simon Sawicki (yt-dlp signing key)"),
/// fingerprint AC0C BBE6 848D 6A87 3464  AF4E 57CF 6593 3B5A 7581, from
/// https://github.com/yt-dlp/yt-dlp/blob/master/public.key (same fingerprint
/// on keys.openpgp.org). RSA 4096, created 2023-02-26.
pub const YTDLP_FINGERPRINT: &str = "AC0CBBE6848D6A873464AF4E57CF65933B5A7581";
#[cfg(test)]
const YTDLP_KEY_CREATED: u32 = 0x63fb_f02e;
const YTDLP_KEY_E: u32 = 65537;
const YTDLP_KEY_N: &str = "\
f4ac5f738c63c0b74b6196de42d5e6f371c0155fb35bd6ff9ea908e4f961197531135428375803399e002b947766fdf6\
6acd480efa9018ac1eb418a7a0edf7d40dff92d1714a11975e44b61e01aaaa2a14a26d56a50e3bce04ecedbb75bad3b1\
cb113400a80ef04cfb88b765aa9a92fad0d21ccabdbb2f1aa681d37798dbd9da22cef9b3f49ca47f0be3e013d1f8cadc\
b00dad76cccc7a3e66429d538e3aee97644166754c71750f5f143a9df4b701298d8b0ce712628d6c698d185a26561edc\
d5a19bb590ece39d2e458eaff204724b2260c7ee93711e6eecfcf9dababa66405b4316bba4b7eda58439e9294986aff2\
3fdc81ea1ff5fef673e84b8ba2a90e47a5b941f012bee167e96a2120ef92e7bee38f7d920f298c976bfd00ed0a233308\
0eac3f3ca334fe54290a92427e8a35d9ee01cd8e670169474a2bb7c320439823adcbc9ce905a41904e28057df4945d61\
c308ef5a5cd84fdde8ea15680632d094b2b6e4709916b9663bab9011cbfd391db48cdcae0353d7aab67c66895d5643c9\
d7b88f49809411be1e4f4e49f2e4bb71ab3726aaf58ef538c705baecd7804f10ef5f717c4a2ff08505bcd70d2bef0200\
2b76aaa53289dc43fca271395f2faab327c10a162a96d7840c4dd027e92bae653a1d5c24e2227c284fb5a38ee14f28a7\
6f7c7d463f86e7a9dcd39f32f10920c812d7f27cfb085fef4623b26493fcd98d";

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

/// Is `sig` (the raw `.sig` file) a valid signature by yt-dlp's key over `data`?
pub fn verify_ytdlp(data: &[u8], sig: &[u8]) -> Result<()> {
    verify(
        data,
        sig,
        &unhex(YTDLP_KEY_N),
        YTDLP_KEY_E,
        YTDLP_FINGERPRINT,
    )
}

fn verify(data: &[u8], sig: &[u8], n: &[u8], e: u32, fingerprint: &str) -> Result<()> {
    let body = signature_packet(sig)?;
    // v4 signature: version, type, public-key algorithm, hash algorithm,
    // hashed subpackets, unhashed subpackets, 2 bytes of the hash, the MPI.
    ensure!(body.len() > 6, "signature too short");
    let (version, sig_type, pk_algo, hash_algo) = (body[0], body[1], body[2], body[3]);
    ensure!(version == 4, "unsupported signature version {version}");
    ensure!(
        sig_type == 0x00,
        "not a signature over a file (type {sig_type})"
    );
    ensure!(pk_algo == 1, "not an RSA signature (algorithm {pk_algo})");
    let hashed_len = u16::from_be_bytes([body[4], body[5]]) as usize;
    let hashed_end = 6 + hashed_len;
    ensure!(body.len() >= hashed_end + 2, "signature truncated");
    let hashed = &body[6..hashed_end];
    let unhashed_len = u16::from_be_bytes([body[hashed_end], body[hashed_end + 1]]) as usize;
    let unhashed_end = hashed_end + 2 + unhashed_len;
    ensure!(body.len() >= unhashed_end + 4, "signature truncated");
    let unhashed = &body[hashed_end + 2..unhashed_end];
    let left16 = &body[unhashed_end..unhashed_end + 2];
    let mpi = &body[unhashed_end + 2..];
    let bits = u16::from_be_bytes([mpi[0], mpi[1]]) as usize;
    let s = mpi
        .get(2..2 + bits.div_ceil(8))
        .context("signature truncated")?;

    // The issuer must be the pinned key (a mismatch here is a clearer error
    // than the RSA check failing).
    if let Some(issuer) = issuer_fingerprint(hashed).or_else(|| issuer_fingerprint(unhashed)) {
        ensure!(
            issuer.eq_ignore_ascii_case(fingerprint),
            "signed with a different key ({issuer})"
        );
    }

    // What is signed: the data, then the signature's own header and hashed
    // subpackets, then the v4 trailer 0x04 0xFF + that length (RFC 4880 5.2.4).
    let header = &body[..hashed_end];
    let mut trailer = vec![0x04, 0xff];
    trailer.extend_from_slice(&(header.len() as u32).to_be_bytes());
    let (digest, scheme) = match hash_algo {
        8 => {
            let d = Sha256::new()
                .chain_update(data)
                .chain_update(header)
                .chain_update(&trailer)
                .finalize();
            (d.to_vec(), Pkcs1v15Sign::new::<Sha256>())
        }
        10 => {
            let d = Sha512::new()
                .chain_update(data)
                .chain_update(header)
                .chain_update(&trailer)
                .finalize();
            (d.to_vec(), Pkcs1v15Sign::new::<Sha512>())
        }
        other => bail!("unsupported hash algorithm {other}"),
    };
    ensure!(digest[..2] == *left16, "signature does not match the file");

    let n_bits = (n.len() * 8) as u32;
    let key = PublicKey::new(
        BoxedUint::from_be_slice(n, n_bits).context("bad key")?,
        BoxedUint::from_be_slice(&e.to_be_bytes(), 32).context("bad key")?,
    )
    .context("bad key")?;
    // RSA signatures are left-padded to the key size; OpenPGP strips leading zeros.
    let mut padded = vec![0u8; n.len().saturating_sub(s.len())];
    padded.extend_from_slice(s);
    key.verify(scheme, &digest, &padded)
        .map_err(|_| anyhow::anyhow!("signature does not match the file"))
}

/// The body of the first packet in `sig`, which must be a signature packet.
fn signature_packet(sig: &[u8]) -> Result<&[u8]> {
    let ctb = *sig.first().context("empty signature")?;
    ensure!(ctb & 0x80 != 0, "not an OpenPGP signature");
    let (tag, len, start) = if ctb & 0x40 == 0 {
        // old format: tag in bits 5..2, length type in bits 1..0
        let tag = (ctb >> 2) & 0x0f;
        let n = match ctb & 3 {
            0 => 1,
            1 => 2,
            2 => 4,
            _ => bail!("indeterminate packet length"),
        };
        let len = sig
            .get(1..1 + n)
            .context("signature truncated")?
            .iter()
            .fold(0usize, |a, b| a << 8 | *b as usize);
        (tag, len, 1 + n)
    } else {
        let tag = ctb & 0x3f;
        let b0 = *sig.get(1).context("signature truncated")? as usize;
        match b0 {
            0..=191 => (tag, b0, 2),
            192..=223 => {
                let b1 = *sig.get(2).context("signature truncated")? as usize;
                (tag, ((b0 - 192) << 8) + b1 + 192, 3)
            }
            255 => {
                let b = sig.get(2..6).context("signature truncated")?;
                (
                    tag,
                    u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize,
                    6,
                )
            }
            _ => bail!("partial packet lengths aren't used for signatures"),
        }
    };
    ensure!(tag == 2, "not a signature packet (tag {tag})");
    sig.get(start..start + len).context("signature truncated")
}

/// Issuer fingerprint subpacket (type 33, v4 key) in a subpacket area.
fn issuer_fingerprint(mut area: &[u8]) -> Option<String> {
    while !area.is_empty() {
        let (len, skip) = match area[0] as usize {
            l @ 0..=191 => (l, 1),
            l @ 192..=254 => (((l - 192) << 8) + *area.get(1)? as usize + 192, 2),
            _ => {
                let b = area.get(1..5)?;
                (u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize, 5)
            }
        };
        let sub = area.get(skip..skip + len)?;
        area = &area[skip + len..];
        if sub.first().map(|t| t & 0x7f) == Some(33) && sub.get(1) == Some(&4) && sub.len() == 22 {
            return Some(sub[2..].iter().map(|b| format!("{b:02X}")).collect());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMS: &[u8] = include_bytes!("testdata/yt-dlp-SHA2-256SUMS");
    const SIG: &[u8] = include_bytes!("testdata/yt-dlp-SHA2-256SUMS.sig");

    /// The pinned numbers really are the key with the published fingerprint
    /// (v4 fingerprint = SHA-1 of the public-key packet, RFC 4880 12.2).
    #[test]
    fn pinned_key_matches_fingerprint() {
        let n = unhex(YTDLP_KEY_N);
        let e = YTDLP_KEY_E.to_be_bytes();
        let e = &e[e.iter().position(|b| *b != 0).unwrap()..];
        let mpi = |v: &[u8]| {
            let bits = v.len() * 8 - v[0].leading_zeros() as usize;
            let mut m = (bits as u16).to_be_bytes().to_vec();
            m.extend_from_slice(v);
            m
        };
        let mut body = vec![4];
        body.extend_from_slice(&YTDLP_KEY_CREATED.to_be_bytes());
        body.push(1);
        body.extend(mpi(&n));
        body.extend(mpi(e));
        let mut h = sha1::Sha1::new();
        h.update([0x99]);
        h.update((body.len() as u16).to_be_bytes());
        h.update(&body);
        let fp: String = h.finalize().iter().map(|b| format!("{b:02X}")).collect();
        assert_eq!(fp, YTDLP_FINGERPRINT);
    }

    #[test]
    fn accepts_a_real_release() {
        verify_ytdlp(SUMS, SIG).unwrap();
    }

    #[test]
    fn rejects_a_changed_file() {
        let mut sums = SUMS.to_vec();
        sums[0] ^= 1; // one changed hash digit
        assert!(verify_ytdlp(&sums, SIG).is_err());
        let mut more = SUMS.to_vec();
        more.extend_from_slice(b"0000  evil.exe\n");
        assert!(verify_ytdlp(&more, SIG).is_err());
    }

    #[test]
    fn rejects_a_changed_signature() {
        let mut sig = SIG.to_vec();
        let last = sig.len() - 1;
        sig[last] ^= 1;
        assert!(verify_ytdlp(SUMS, &sig).is_err());
        assert!(verify_ytdlp(SUMS, &SIG[..SIG.len() / 2]).is_err());
        assert!(verify_ytdlp(SUMS, b"").is_err());
        assert!(verify_ytdlp(SUMS, b"-----BEGIN PGP SIGNATURE-----").is_err());
    }

    #[test]
    fn rejects_another_key() {
        // Same signature, but checked against a different (here: altered) key.
        let mut n = unhex(YTDLP_KEY_N);
        n[100] ^= 1;
        assert!(verify(SUMS, SIG, &n, YTDLP_KEY_E, YTDLP_FINGERPRINT).is_err());
        let err = verify(
            SUMS,
            SIG,
            &unhex(YTDLP_KEY_N),
            YTDLP_KEY_E,
            "00".repeat(20).as_str(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("different key"), "{err}");
    }
}
