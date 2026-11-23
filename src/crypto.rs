//! ChaCha20 (RFC 8439) e utilità per le password salvate su disco.
//!
//! Il progetto compila offline e non ha un crate di cifratura fra le
//! dipendenze: l'algoritmo è scritto qui (poche decine di righe) ed è
//! verificato con il vettore di test della RFC. La chiave non vive nel
//! database: sta nel portachiavi di sistema (vedi `keyring.rs`).

/// Costante di espansione «expand 32-byte k».
const SIGMA: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

fn quarter_round(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] ^= state[a];
    state[d] = state[d].rotate_left(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] ^= state[c];
    state[b] = state[b].rotate_left(12);
    state[a] = state[a].wrapping_add(state[b]);
    state[d] ^= state[a];
    state[d] = state[d].rotate_left(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] ^= state[c];
    state[b] = state[b].rotate_left(7);
}

/// Blocco di keystream (64 byte) per `counter` e `nonce` dati.
fn block(key: &[u8; 32], counter: u32, nonce: &[u8; 12]) -> [u8; 64] {
    let mut state = [0u32; 16];
    state[..4].copy_from_slice(&SIGMA);
    for (index, chunk) in key.chunks_exact(4).enumerate() {
        state[4 + index] = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    state[12] = counter;
    for (index, chunk) in nonce.chunks_exact(4).enumerate() {
        state[13 + index] = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }

    let initial = state;
    for _ in 0..10 {
        quarter_round(&mut state, 0, 4, 8, 12);
        quarter_round(&mut state, 1, 5, 9, 13);
        quarter_round(&mut state, 2, 6, 10, 14);
        quarter_round(&mut state, 3, 7, 11, 15);
        quarter_round(&mut state, 0, 5, 10, 15);
        quarter_round(&mut state, 1, 6, 11, 12);
        quarter_round(&mut state, 2, 7, 8, 13);
        quarter_round(&mut state, 3, 4, 9, 14);
    }

    let mut output = [0u8; 64];
    for index in 0..16 {
        let word = state[index].wrapping_add(initial[index]);
        output[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    output
}

/// XOR dei dati con il keystream: la stessa operazione cifra e decifra.
pub fn apply_keystream(key: &[u8; 32], nonce: &[u8; 12], data: &mut [u8]) {
    let mut counter = 0u32;
    for chunk in data.chunks_mut(64) {
        let keystream = block(key, counter, nonce);
        for (byte, mask) in chunk.iter_mut().zip(keystream.iter()) {
            *byte ^= mask;
        }
        counter = counter.wrapping_add(1);
    }
}

/// Cifra `plaintext` con un nonce casuale nuovo: il nonce va conservato
/// accanto al testo cifrato (non è segreto).
pub fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> std::io::Result<([u8; 12], Vec<u8>)> {
    let nonce = random_bytes::<12>()?;
    let mut ciphertext = plaintext.to_vec();
    apply_keystream(key, &nonce, &mut ciphertext);
    Ok((nonce, ciphertext))
}

pub fn decrypt(key: &[u8; 32], nonce: &[u8; 12], ciphertext: &[u8]) -> Vec<u8> {
    let mut plaintext = ciphertext.to_vec();
    apply_keystream(key, nonce, &mut plaintext);
    plaintext
}

/// Byte casuali dal generatore del sistema operativo.
pub fn random_bytes<const N: usize>() -> std::io::Result<[u8; N]> {
    use std::io::Read;

    let mut buffer = [0u8; N];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buffer)?;
    Ok(buffer)
}

pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if text.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(text.len() / 2);
    let digits = text.as_bytes();
    for pair in digits.chunks_exact(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        bytes.push((high * 16 + low) as u8);
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc8439_block_test_vector() {
        // RFC 8439, § 2.3.2.
        let key: [u8; 32] = core::array::from_fn(|index| index as u8);
        // Nonce 000000090000004a00000000.
        let mut nonce = [0u8; 12];
        nonce[3] = 0x09;
        nonce[7] = 0x4a;
        assert_eq!(
            to_hex(&block(&key, 1, &nonce)),
            "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e\
             d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e"
        );
    }

    #[test]
    fn encryption_roundtrips_and_uses_a_fresh_nonce() {
        let key = [7u8; 32];
        let (nonce_one, ciphertext) = encrypt(&key, "password segreta".as_bytes()).unwrap();
        let (nonce_two, other) = encrypt(&key, "password segreta".as_bytes()).unwrap();
        assert_ne!(nonce_one, nonce_two);
        assert_ne!(ciphertext, other);
        assert_eq!(
            String::from_utf8(decrypt(&key, &nonce_one, &ciphertext)).unwrap(),
            "password segreta"
        );
    }

    #[test]
    fn hex_roundtrip() {
        let bytes = [0u8, 1, 15, 16, 255];
        assert_eq!(from_hex(&to_hex(&bytes)).unwrap(), bytes);
        assert!(from_hex("abc").is_none());
        assert!(from_hex("zz").is_none());
    }
}
