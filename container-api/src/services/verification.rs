//! One-time codes sent by email: generation, hashing and the checks every
//! verification flow shares. Same code as the signup backend's verification.rs
//! (which handles activation); here it guards account deletion.
//!
//! A code is six digits so it is easy to read from a phone and type into a
//! terminal. Brute force is bounded by `MAX_ATTEMPTS` guesses per code and a
//! short lifetime, which together make guessing hopeless (5 in 1,000,000).
//! Only `sha256(id ":" code)` is stored, so a database dump holds no usable codes.

use chrono::{DateTime, Duration, Utc};
use rand::Rng;
use sha2::{Digest, Sha256};

pub const CODE_LEN: usize = 6;
pub const MAX_ATTEMPTS: i32 = 5;
pub const LIFETIME_MINUTES: i64 = 30;
/// Minimum gap between two codes for the same verification (resend cooldown).
pub const RESEND_COOLDOWN_SECONDS: i64 = 60;

pub fn generate_code() -> String {
    let n: u32 = rand::thread_rng().gen_range(0..1_000_000);
    format!("{n:06}")
}

pub fn hash_code(verification_id: &str, code: &str) -> String {
    let mut h = Sha256::new();
    h.update(verification_id.as_bytes());
    h.update(b":");
    h.update(normalize(code).as_bytes());
    format!("{:x}", h.finalize())
}

/// What people paste: digits, possibly with spaces or a dash in the middle.
pub fn normalize(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_digit()).collect()
}

pub fn expiry_from(now: DateTime<Utc>) -> DateTime<Utc> {
    now + Duration::minutes(LIFETIME_MINUTES)
}

/// State of a stored verification row as far as a check is concerned.
#[derive(Debug, Clone)]
pub struct Stored {
    pub id: String,
    pub code_hash: String,
    pub attempts: i32,
    pub expires_at: DateTime<Utc>,
    pub verified_at: Option<DateTime<Utc>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    /// Code matches (or was already accepted with the same code).
    Ok,
    /// Wrong code; the number of guesses still allowed.
    Wrong { attempts_left: i32 },
    /// Lifetime passed or too many wrong guesses: a new code is needed.
    Dead,
}

pub fn check(stored: &Stored, code: &str, now: DateTime<Utc>) -> Check {
    if stored.attempts >= MAX_ATTEMPTS || now > stored.expires_at {
        return Check::Dead;
    }
    if normalize(code).len() != CODE_LEN {
        return Check::Wrong {
            attempts_left: MAX_ATTEMPTS - stored.attempts - 1,
        };
    }
    if constant_time_eq(
        hash_code(&stored.id, code).as_bytes(),
        stored.code_hash.trim().as_bytes(),
    ) {
        Check::Ok
    } else {
        Check::Wrong {
            attempts_left: MAX_ATTEMPTS - stored.attempts - 1,
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(code: &str, attempts: i32, minutes_left: i64) -> Stored {
        Stored {
            id: "v1".into(),
            code_hash: hash_code("v1", code),
            attempts,
            expires_at: Utc::now() + Duration::minutes(minutes_left),
            verified_at: None,
        }
    }

    #[test]
    fn code_is_six_digits() {
        for _ in 0..50 {
            let c = generate_code();
            assert_eq!(c.len(), 6);
            assert!(c.chars().all(|ch| ch.is_ascii_digit()));
        }
    }

    #[test]
    fn accepts_right_code_with_spaces() {
        let s = stored("123456", 0, 10);
        assert_eq!(check(&s, "123 456", Utc::now()), Check::Ok);
        assert_eq!(check(&s, "123-456", Utc::now()), Check::Ok);
    }

    #[test]
    fn rejects_wrong_code_and_counts_down() {
        let s = stored("123456", 3, 10);
        assert_eq!(
            check(&s, "000000", Utc::now()),
            Check::Wrong { attempts_left: 1 }
        );
    }

    #[test]
    fn dead_after_expiry_or_attempts() {
        assert_eq!(
            check(&stored("123456", 0, -1), "123456", Utc::now()),
            Check::Dead
        );
        assert_eq!(
            check(&stored("123456", 5, 10), "123456", Utc::now()),
            Check::Dead
        );
    }

    #[test]
    fn hash_is_bound_to_the_row() {
        assert_ne!(hash_code("a", "123456"), hash_code("b", "123456"));
    }
}
