//! Deterministic stand-in for `openai/text-embedding-3-small`.
//!
//! Hutch-lite calls OpenRouter for real vectors. The mock and the
//! threshold test use this geometry so a cosine cutoff can be measured
//! without a key. Each content word is an axis. A shared component keeps
//! unrelated notes from sitting at cosine 0, which is what real
//! embedding models do.
//!
//! The default ABOUT threshold is the gap between the fixture pairs in
//! [`tests::threshold_sits_between_related_and_unrelated`]. It is also
//! the starting cutoff for the live model: related short notes land
//! above it and unrelated notes land below it in this geometry. A live
//! pass can raise it if a loose criterion starts matching invoices.

use unicode_normalization::UnicodeNormalization;

/// OpenRouter slug. List price is $0.02 per million input tokens.
pub const EMBED_MODEL: &str = "openai/text-embedding-3-small";

/// USD per million input tokens, from OpenRouter's published price.
pub const EMBED_USD_PER_MILLION: f64 = 0.02;

/// Cosine cutoff for `ABOUT`. Measured on the fixture table below.
pub const ABOUT_THRESHOLD: f64 = 0.40;

const DIM: usize = 64;
/// Weight of the component every non-empty text shares.
const SHARED: f64 = 0.8;

const STOP: &[&str] = &[
    "the", "and", "for", "are", "was", "were", "with", "from", "that", "this", "have", "has", "had", "not",
    "but", "you", "your", "our", "any", "all", "can", "its", "his", "her", "she", "him", "they", "them",
];

/// Trim, case-fold, and collapse whitespace. The embed cache key uses this
/// so `Biped` and `biped` are one vector.
pub fn normalize_embed_text(text: &str) -> String {
    let nfc: String = text.trim().nfc().collect();
    let mut out = String::new();
    let mut spaced = false;
    for c in nfc.chars() {
        if c.is_whitespace() {
            spaced = !out.is_empty();
            continue;
        }
        if spaced {
            out.push(' ');
            spaced = false;
        }
        for low in c.to_lowercase() {
            out.push(low);
        }
    }
    out
}

/// Unit vector for `text`. Empty text is the zero vector, so it never
/// matches. Identical normalized text is cosine 1.
pub fn mock_embedding(text: &str) -> Vec<f64> {
    let norm = normalize_embed_text(text);
    let words = content_words(&norm);
    if words.is_empty() {
        return vec![0.0; DIM];
    }
    let mut v = vec![0.0; DIM];
    v[0] = SHARED;
    for word in words {
        let bucket = 1 + (fnv(&word) as usize % (DIM - 1));
        v[bucket] += 1.0;
    }
    l2_normalize(&mut v);
    v
}

/// Cosine similarity in `[-1, 1]`. A zero vector, or a length mismatch,
/// is 0.
pub fn cosine(a: &[f64], b: &[f64]) -> f64 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut na = 0.0;
    let mut nb = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= f64::EPSILON || nb <= f64::EPSILON {
        return 0.0;
    }
    (dot / na.sqrt() / nb.sqrt()).clamp(-1.0, 1.0)
}

fn content_words(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() >= 3 && !STOP.contains(w))
        .collect()
}

fn fnv(word: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in word.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn l2_normalize(v: &mut [f64]) {
    let n: f64 = v.iter().map(|x| x * x).sum::<f64>().sqrt();
    if n <= f64::EPSILON {
        return;
    }
    for x in v.iter_mut() {
        *x /= n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sim(a: &str, b: &str) -> f64 {
        cosine(&mock_embedding(a), &mock_embedding(b))
    }

    #[test]
    fn threshold_sits_between_related_and_unrelated() {
        let related = [
            ("large hairy biped crossed the road", "biped"),
            ("a sasquatch was seen near the ridge", "sasquatch"),
            ("large hairy biped crossed the road", "hairy biped"),
            ("large hairy biped crossed the road", "Large Hairy Biped crossed the road"),
        ];
        let unrelated = [
            ("invoice 12 for services rendered", "biped"),
            ("the quarterly budget meeting is tuesday", "sasquatch"),
            ("please send the attached spreadsheet", "hairy biped"),
            ("invoice 12 for services rendered", "large hairy biped crossed the road"),
        ];
        let mut low_related = f64::MAX;
        let mut high_unrelated = f64::MIN;
        for (a, b) in related {
            let s = sim(a, b);
            low_related = low_related.min(s);
            assert!(
                s > ABOUT_THRESHOLD,
                "{a:?} ~ {b:?} cosine {s:.4} is not above {ABOUT_THRESHOLD}"
            );
        }
        for (a, b) in unrelated {
            let s = sim(a, b);
            high_unrelated = high_unrelated.max(s);
            assert!(
                s < ABOUT_THRESHOLD,
                "{a:?} ~ {b:?} cosine {s:.4} is not below {ABOUT_THRESHOLD}"
            );
        }
        assert!(
            high_unrelated < ABOUT_THRESHOLD && ABOUT_THRESHOLD < low_related,
            "unrelated max {high_unrelated:.4}, threshold {ABOUT_THRESHOLD}, related min {low_related:.4}"
        );
        assert!((sim("biped", "biped") - 1.0).abs() < 1e-9);
        assert_eq!(sim("", "biped"), 0.0);
    }
}
