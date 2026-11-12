use rand::RngCore;

const CHARACTER_MAP: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// Equivalent of `Filesystem::readRandomCharacters()`: maps the high 5 bits of
/// each random byte onto `[a-z2-7]`.
pub fn random_characters(count: usize) -> String {
    let mut bytes = vec![0u8; count];
    rand::rng().fill_bytes(&mut bytes);
    characters_from_bytes(&bytes)
}

fn characters_from_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| char::from(CHARACTER_MAP[usize::from(byte >> 3)]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_high_bits() {
        assert_eq!(characters_from_bytes(&[0x00, 0x07, 0x08, 0xff]), "aab7");
    }

    #[test]
    fn produces_requested_length_and_alphabet() {
        let value = random_characters(40);
        assert_eq!(value.len(), 40);
        assert!(value.bytes().all(|byte| CHARACTER_MAP.contains(&byte)));
    }
}
