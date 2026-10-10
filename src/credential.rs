//! **How the operator's password is stored, and how a login is checked.**
//!
//! One home for the rule, because three surfaces authenticate against the
//! same credential — telnet (`telnet/session.rs`), the SSH server (`ssh.rs`)
//! and the web config UI (`webserver.rs`) — and a rule written in three
//! places holds in one.
//!
//! `egateway.conf`'s `password` may be **either** form:
//!
//! * a **PBKDF2 PHC string** (`$pbkdf2-sha256$i=...$salt$hash`), which is what
//!   the startup migration writes and what every config surface now stores, or
//! * **cleartext**, which is what every release up to 1.0.0-RC1 wrote and
//!   what the shipped default still is.
//!
//! **Accepting both is not laziness, it is the upgrade path.** Rejecting
//! cleartext would lock every existing installation out of its own gateway
//! at the version that introduced hashing, on a device that is often
//! headless and reached from a Commodore 64. `verify` therefore takes either.
//!
//! An existing installation does not have to wait for its owner to notice:
//! `load_or_create_config` rewrites a cleartext password as a hash on the
//! **next start** (it rewrites the file at that point anyway, to add keys a new
//! version introduced), and every surface that *sets* a password stores it
//! hashed. `needs_rehash` is what both of those ask; no screen displays it,
//! because after that first restart there is nothing left for a screen to
//! report.
//!
//! **The shipped default (`changeme`) stays cleartext deliberately.** A
//! salted hash differs on every write, so a hashed default could not be
//! written down: `usermanual.html`'s Key/Default table is compared against a
//! config rendered from `Config::default()` by
//! `config::tests::test_the_manual_sample_config_matches_the_real_defaults`,
//! and a value that changes per write cannot be documented or compared.
//! `changeme` is a published placeholder rather than a secret, so nothing is
//! protected by hashing it — what matters is that the password an operator
//! *chooses* never lands on disk in the clear.
//!
//! **PBKDF2-HMAC-SHA256, and the reason is the hardware.** Argon2id is the
//! stronger algorithm and is equally already in the tree, but its recommended
//! parameters ask for **19 MiB per verification** — measured here at ~1 s per
//! login in a debug build — and that cost is *per concurrent attempt*. This
//! gateway runs on Raspberry Pis with `max_sessions` defaulting to 50, so a
//! burst of telnet logins could ask for most of the machine's RAM. PBKDF2
//! costs kilobytes, is named in the OpenSSF criterion's approved list, and its
//! whole cost is one tunable number.
//!
//! Iterations are recorded *inside* the stored string
//! (`$pbkdf2-sha256$i=...$salt$hash`), and verification takes its parameters
//! from the stored hash rather than from `PBKDF2_ROUNDS`, so raising the count
//! later does not invalidate a single existing credential.

use crate::glog;
#[cfg(test)]
use std::sync::atomic::AtomicU32;
#[cfg(test)]
use std::sync::atomic::AtomicU64;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use pbkdf2::{
    password_hash::{phc::PasswordHash, PasswordHasher, PasswordVerifier},
    Algorithm, Params, Pbkdf2,
};

/// Iterations for a newly stored password.
///
/// The crate's own `Params::RECOMMENDED_ROUNDS` is 600,000 (OWASP's figure for
/// PBKDF2-HMAC-SHA256 on server hardware). This project's floor is a Raspberry
/// Pi, where that lands in the seconds, so the number here is **measured on
/// this hardware and pinned by
/// `test_a_login_costs_what_we_think_it_costs`** rather than copied from a
/// recommendation written for a different machine.
///
/// It is safe to raise: the value used to verify comes from the stored string,
/// so old credentials keep working and are re-hashed at the new count the next
/// time the operator sets a password.
const PBKDF2_ROUNDS: u32 = 210_000;

/// Derived-key length, in bytes. 32 is the crate's recommendation and a full
/// SHA-256 output; there is no reason to store less.
const PBKDF2_OUTPUT_LEN: usize = 32;

/// Is `stored` a hash rather than a literal password?
///
/// Tests for the **PHC string shape** (`$<algorithm>$...`) rather than for our
/// own algorithm, and the asymmetry is deliberate: mistaking a hash for
/// cleartext is the dangerous direction, because the file's own contents would
/// then become a password anyone who can read it could present. So anything
/// shaped like a PHC string is treated as a hash — `verify` refuses the ones
/// it cannot check rather than falling back to a literal comparison.
///
/// The cost is that a *cleartext* password shaped like `$foo$bar` would be
/// read as a hash and refused -- the safe failure (a login that does not work,
/// rather than a file that authenticates itself).  The migration cannot remove
/// that case, because to it the value already *is* a hash, so it is closed at
/// the other end: a password an operator types never reaches this test. Every
/// surface stores a typed value through [`store_typed`], which hashes it
/// whatever it looks like. Only a value hand-edited into `egateway.conf` can
/// still be misread this way.
pub(crate) fn is_hashed(stored: &str) -> bool {
    let rest = match stored.strip_prefix('$') {
        Some(r) => r,
        None => return false,
    };
    match rest.split_once('$') {
        // A non-empty algorithm identifier, of the characters PHC allows.
        Some((algo, _)) => {
            !algo.is_empty()
                && algo
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        }
        None => false,
    }
}

/// The stored form of a password an operator has just **typed**.
///
/// A typed value is plaintext by definition, so it is hashed whatever its
/// shape -- unlike [`hash_if_cleartext`], which reads a value out of the file
/// and cannot tell `$ecret$2024` from a hash.  Routing a typed `$ecret$2024`
/// through that test stored it in the clear, where `verify` then refused every
/// login: a lockout on a headless box.  Empty stays empty ("leave it alone" /
/// "no password"), since a hash of the empty string would accept an empty
/// login.  If hashing fails the value is kept as typed, as `hash_if_cleartext`
/// does, rather than blanked.
pub(crate) fn store_typed(plain: &str) -> String {
    if plain.is_empty() {
        return String::new();
    }
    match hash(plain) {
        Some(h) => h,
        None => {
            glog!("Warning: could not hash the new password; it is stored as typed.");
            plain.to_string()
        }
    }
}

/// Why `name` cannot be the login username, if it cannot -- one rule for the
/// wizard, telnet, the web UI and the desktop editor.  A `:` is refused because
/// HTTP Basic auth splits `user:password` at the first colon, so the web UI
/// could never accept that name; whitespace and an empty name for the reasons
/// the wizard always gave.  Judged on the trimmed name, as it is stored.
pub(crate) fn username_problem(name: &str) -> Option<&'static str> {
    let name = name.trim();
    if name.is_empty() {
        Some("Username cannot be empty.")
    } else if name.contains(char::is_whitespace) {
        Some("Username cannot contain spaces.")
    } else if name.contains(':') {
        Some("Username cannot contain ':'.")
    } else {
        None
    }
}

/// The value to store for config key `key` when an operator typed `typed` --
/// the password hashed by [`store_typed`], the username trimmed, anything else
/// as typed.  For the
/// screens that set one key at a time (telnet's Security menu).
pub(crate) fn typed_config_value(key: &str, typed: String) -> String {
    match key {
        "password" => store_typed(&typed),
        // Judged trimmed by `username_problem`, so stored trimmed.
        "username" => typed.trim().to_string(),
        _ => typed,
    }
}

/// Does this stored credential still need rehashing?
///
/// True when it is a non-empty cleartext value — i.e. an installation that
/// predates hashing, or one still on the shipped default. An empty password is
/// not "needs rehashing": it is "no password set", which the callers already
/// treat as refuse-everything (see `ssh.rs`'s empty-credential guard).
pub(crate) fn needs_rehash(stored: &str) -> bool {
    !stored.is_empty() && !is_hashed(stored)
}

/// Hash a password for storage, as a PBKDF2-HMAC-SHA256 PHC string with a
/// random per-password salt.
///
/// Returns `None` if hashing fails, which the caller must treat as "do not
/// store anything" rather than falling back to cleartext — silently writing
/// the password in the clear because the hasher hiccuped is the one outcome
/// this module exists to prevent.
pub(crate) fn hash(plain: &str) -> Option<String> {
    hash_with_rounds(plain, rounds())
}

/// The round count a new hash is stored at: `PBKDF2_ROUNDS`, unless a test has
/// turned it down.
///
/// The override exists because the *wiring* tests and the *cost* test want
/// opposite things. `config::tests`' two writer tests ask "does this save path
/// hash at all?", and answering it at the production count costs ~16 s each in
/// a debug build -- a third of the whole suite for a question that does not
/// depend on the number. `test_a_login_costs_what_we_think_it_costs` asks the
/// opposite question and must pay in full. `CheapRounds` serialises the two so
/// neither can see the other's setting.
fn rounds() -> u32 {
    #[cfg(test)]
    {
        let r = TEST_ROUNDS.load(Ordering::Relaxed);
        if r != 0 {
            return r;
        }
    }
    PBKDF2_ROUNDS
}

/// `hash`, with the iteration count supplied.
///
/// Exists so the tests can exercise the *logic* -- round-trip, salting,
/// refusal -- at a few rounds instead of `PBKDF2_ROUNDS`. A debug build costs
/// ~5.4 s per production-count hash on this machine, and the handful of tests
/// below would otherwise add well over a minute to a suite that was
/// deliberately cut from 539 s to 53 s. Exactly one test
/// (`test_a_login_costs_what_we_think_it_costs`) pays the real price, which is
/// the one that is measuring it.
fn hash_with_rounds(plain: &str, rounds: u32) -> Option<String> {
    let params = Params::new_with_output_len(rounds, PBKDF2_OUTPUT_LEN).ok()?;
    let hasher = Pbkdf2::new(Algorithm::Pbkdf2Sha256, params);
    hasher
        .hash_password(plain.as_bytes())
        .ok()
        .map(|h| h.to_string())
}

/// Check a supplied password against the stored credential.
///
/// Handles both storage forms. An **empty stored credential never matches**,
/// including against an empty supplied password: `constant_time_eq(b"", b"")`
/// is `true`, so without this a blanked password would turn an
/// authenticating listener into an accept-anything one. Each caller guards
/// that too (deliberately — defence in depth on the SSH port, which binds
/// `0.0.0.0`), and it is repeated here so a future caller cannot forget.
pub(crate) fn verify(stored: &str, supplied: &str) -> bool {
    if stored.is_empty() {
        return false;
    }
    if is_hashed(stored) {
        #[cfg(test)]
        DERIVED_ON
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((supplied.to_owned(), std::thread::current().id()));
        // Parameters (including the iteration count) come from the stored
        // string, not from `PBKDF2_ROUNDS`, so raising that constant later
        // leaves every existing credential working.
        match PasswordHash::new(stored) {
            // `Pbkdf2::SHA256` supplies only the *fallback* algorithm and
            // parameters; `password_hash`'s blanket verifier reads the
            // algorithm, salt and iteration count out of `parsed` itself, and
            // refuses (rather than guesses) an algorithm it cannot compute.
            Ok(parsed) => Pbkdf2::SHA256
                .verify_password(supplied.as_bytes(), &parsed)
                .is_ok(),
            // A malformed or unsupported PHC string is a broken credential,
            // not a literal password: refuse rather than fall through to a
            // cleartext comparison, or the hash text itself would become a
            // password anyone who can read the file could present.
            Err(_) => false,
        }
    } else {
        crate::telnet::constant_time_eq(supplied.as_bytes(), stored.as_bytes())
    }
}

/// Replace a cleartext password in place with its PBKDF2 hash.
///
/// Idempotent: an already-hashed value is left alone, so calling it on every
/// save cannot double-hash. An empty value is left alone too — that means "no
/// password set", and hashing the empty string would turn "refuse everything"
/// into a credential someone could actually present.
///
/// Called from the two places an operator's password reaches the config —
/// `config::update_config_values` (telnet and the web UI, which set the
/// `password` key) and `config::save_config` (the desktop editor and the
/// first-run wizard, which write the whole struct). **Deliberately not called
/// from `config::write_config_file`**, the writer both of those funnel into,
/// because that is also what auto-creates the shipped config: hashing there
/// would give `Config::default()` a salted value that differs on every write,
/// which cannot be documented in `usermanual.html` and cannot be compared by
/// the test that holds the manual to the real defaults.
///
/// **Consequence worth knowing:** once hashed, the password cannot be read
/// back out of `egateway.conf`. An operator configuring a master/slave pair
/// needs the plaintext for the slave's `slave_master_password`, which stays
/// cleartext of necessity — the slave *presents* it, so it can never be a
/// hash. Note the password down when setting it.
pub(crate) fn hash_if_cleartext(stored: &mut String) {
    if !needs_rehash(stored) {
        return;
    }
    match hash(stored) {
        Some(h) => *stored = h,
        // Leave the cleartext rather than blanking it: a blank password is
        // "refuse every login", which would lock the operator out of a
        // headless gateway because a hasher failed.
        None => glog!("Warning: could not hash the configured password; it stays as typed."),
    }
}

/// **The expensive check happens once, when the operator first gets in.**
///
/// HTTP Basic auth re-presents the credential on *every* request, and this
/// server checks it on every request (`webserver::is_authorized`). That is
/// free while the stored password is cleartext and ruinous once it is a
/// PBKDF2 hash: a derivation measured **237 ms in release** on this desktop,
/// against a `/vdm/frame` poll every 150 ms, a `/vdm/list` poll every 2 s, a
/// joystick beat as fast as every 100 ms and a `/logs` refresh every 2 s.
/// That is ~6.7 checked requests a second, so the KDF alone would ask for
/// **more than one core on a desktop** and rather more than a Raspberry Pi
/// has -- and each one occupied a tokio worker, the same runtime that drives
/// the serial pumps and the CP/M boot session's speed governor. The screen a
/// booted disk is watched on would have been paying for itself in emulator
/// stutter. (A miss now derives on the blocking pool instead -- see
/// [`verify_cached_off_runtime`] -- but the cache is still what keeps the
/// *rate* down: a thread hop is cheaper than a KDF, not free.)
///
/// So security gates the way *in*, and a session already inside stays inside:
/// the first request pays the KDF, and every later one presenting the same
/// credential is answered from a digest compare.
///
/// **There is deliberately no expiry.** An entry is keyed on the *stored*
/// credential as well as the supplied one, so changing the password
/// invalidates every cached answer at once -- there is no window in which an
/// old password still opens the web UI, which is the only thing a timeout
/// would have bought. Expiring on a clock would just re-burn 237 ms on a
/// browser that is going to present the same correct password anyway.
///
/// Three things keep that safe rather than merely fast:
///
/// * **Only successes are cached.** A wrong password always pays full price,
///   so the cache is not an oracle that answers faster for a near miss, and
///   guessing stays as expensive as the KDF makes it. The per-IP lockout
///   (3 failures, 5 minutes) still bounds how often an attacker may pay it.
/// * **Cleartext never enters the cache.** `verify` is already a
///   constant-time byte compare in that case, so there is nothing to save.
/// * **The key is a digest, not the password.** One slot suffices because
///   there is exactly one credential that can succeed, so every browser and
///   every poll collapses onto the same entry.
///
/// The one remembered success: a digest binding the stored credential to the
/// supplied one.
static AUTH_CACHE: Mutex<Option<[u8; 32]>> = Mutex::new(None);

/// Counts how many times we actually ran the KDF, so a test can assert the
/// cache *was used* rather than infer it from a stopwatch.
#[cfg(test)]
static DERIVATIONS: AtomicU64 = AtomicU64::new(0);

/// Which thread each hashed verification ran on, keyed by the supplied
/// password, so a test can prove the KDF left the runtime worker without
/// timing anything.
#[cfg(test)]
static DERIVED_ON: Mutex<Vec<(String, std::thread::ThreadId)>> = Mutex::new(Vec::new());

/// Bind the stored credential and the supplied one into one cache key.
///
/// The stored value carries a random salt, so this is not a fast hash of the
/// password alone; and the length prefix keeps `("ab", "c")` from colliding
/// with `("a", "bc")`.
fn cache_key(stored: &str, supplied: &str) -> [u8; 32] {
    use pbkdf2::sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update((stored.len() as u64).to_le_bytes());
    h.update(stored.as_bytes());
    h.update(supplied.as_bytes());
    h.finalize().into()
}

/// Is `key` the one remembered success?
fn cache_hit(key: &[u8; 32]) -> bool {
    let guard = AUTH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    // Constant-time even here: the comparands are digests rather than
    // secrets, but a fast path that leaks by timing is a habit worth not
    // forming.
    guard.as_ref().is_some_and(|cached| crate::telnet::constant_time_eq(cached, key))
}

/// `verify`, for a caller that is handed the credential on every request.
///
/// Use this from the web server only. Telnet and SSH authenticate once per
/// session, where the KDF's cost is a feature rather than a bill. An async
/// caller wants [`verify_cached_off_runtime`], which is this with the
/// derivation moved off the worker.
pub(crate) fn verify_cached(stored: &str, supplied: &str) -> bool {
    // Nothing to save, and nothing we would want to remember.
    if !is_hashed(stored) {
        return verify(stored, supplied);
    }
    let key = cache_key(stored, supplied);
    if cache_hit(&key) {
        return true;
    }
    #[cfg(test)]
    DERIVATIONS.fetch_add(1, Ordering::Relaxed);
    if verify(stored, supplied) {
        let mut guard = AUTH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(key);
        true
    } else {
        false
    }
}

/// **The KDF never runs on a runtime worker.**
///
/// Every authenticating caller is an async task -- the telnet login, russh's
/// `auth_password`, the web server's per-request gate -- and a derivation is
/// 237 ms in release and seconds on a Pi or in a debug build. Run inline, that
/// whole time is a tokio worker doing nothing else, and the same workers drive
/// the serial pumps and the CP/M speed governor: a burst of logins (or of
/// guesses, which always pay) would stall a booted guest and a modem's online
/// loop for as long as the guesses kept coming. `spawn_blocking` moves the
/// derivation to the blocking pool, which exists for exactly this.
///
/// Only the expensive case moves. A cleartext credential is a constant-time
/// byte compare and an empty one is refused outright, so neither is worth a
/// thread hop. If the blocking pool cannot run the task (the runtime is
/// shutting down) the answer is **no** -- a login refused during shutdown
/// costs nothing, while a panic in the auth path would cost the listener.
pub(crate) async fn verify_off_runtime(stored: &str, supplied: &str) -> bool {
    if !is_hashed(stored) {
        return verify(stored, supplied);
    }
    let (stored, supplied) = (stored.to_owned(), supplied.to_owned());
    tokio::task::spawn_blocking(move || verify(&stored, &supplied))
        .await
        .unwrap_or(false)
}

/// [`verify_cached`], with the derivation moved off the runtime the same way.
///
/// The cache is consulted **here, on the worker**, because a hit is a digest
/// compare and the point of the cache is that the polls behind the gate cost
/// nothing -- a thread hop per `/vdm/frame` would be a new cost of its own.
/// Only a miss goes to the blocking pool, and it goes through `verify_cached`
/// itself, so what is remembered (successes only, keyed on the stored
/// credential) is still decided in exactly one place.
pub(crate) async fn verify_cached_off_runtime(stored: &str, supplied: &str) -> bool {
    if !is_hashed(stored) {
        return verify(stored, supplied);
    }
    if cache_hit(&cache_key(stored, supplied)) {
        return true;
    }
    let (stored, supplied) = (stored.to_owned(), supplied.to_owned());
    tokio::task::spawn_blocking(move || verify_cached(&stored, &supplied))
        .await
        .unwrap_or(false)
}

/// Set while a test wants cheap hashing; 0 means "use `PBKDF2_ROUNDS`".
#[cfg(test)]
static TEST_ROUNDS: AtomicU32 = AtomicU32::new(0);

/// Serialises every test that cares what round count is in force -- the ones
/// that turn it down, and the one that measures it at full price.
#[cfg(test)]
static ROUNDS_LOCK: Mutex<()> = Mutex::new(());

/// Turn the production round count down for the life of the guard.
///
/// Hold it across any call that reaches `hash` -- including indirectly,
/// through `config::save_config` or `config::update_config_values`.
#[cfg(test)]
pub(crate) struct CheapRounds {
    /// Held, never read: dropping it is what releases the serialisation.
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl CheapRounds {
    pub(crate) fn new() -> Self {
        let g = ROUNDS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        TEST_ROUNDS.store(Params::MIN_ROUNDS, Ordering::Relaxed);
        Self { _lock: g }
    }
}

#[cfg(test)]
impl Drop for CheapRounds {
    fn drop(&mut self) {
        TEST_ROUNDS.store(0, Ordering::Relaxed);
    }
}

/// A hash at the crate's minimum round count, for tests in *other* modules
/// that need a realistic stored credential without paying ~5.4 s of debug-build
/// KDF for something that is not measuring the KDF.
#[cfg(test)]
pub(crate) fn hash_for_test(plain: &str) -> String {
    hash_with_rounds(plain, Params::MIN_ROUNDS).expect("hashing failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hash at the fewest rounds the crate will accept: enough to exercise the
    /// logic, cheap enough that a debug-build suite does not grow by a minute
    /// (the production count costs ~5.4 s a hash there).  Taken from the
    /// crate's own floor rather than written as a number, so it cannot drift
    /// out of range and turn every test below into "hashing failed".  See
    /// `hash_with_rounds`.
    fn cheap(plain: &str) -> String {
        hash_with_rounds(plain, Params::MIN_ROUNDS).expect("hashing failed")
    }

    #[test]
    fn test_a_login_costs_what_we_think_it_costs() {
        use std::time::Instant;
        // Excludes any test that has turned the round count down; without this
        // the assertion below would fail whenever one happened to overlap.
        let _lock = ROUNDS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(rounds(), PBKDF2_ROUNDS, "something left the round count turned down");
        // The *production* count, which is the whole point of this one test.
        let h = hash("changeme").expect("hash");
        // Pinned from the stored string rather than from the constant: this is
        // what a verification will actually read its parameters from, so a
        // change to `PBKDF2_ROUNDS` that failed to reach the wire would show
        // up here rather than silently costing every operator nothing.
        assert!(
            h.contains(&format!("i={PBKDF2_ROUNDS},l={PBKDF2_OUTPUT_LEN}")),
            "stored parameters are not the ones we configured: {h}"
        );
        let t = Instant::now();
        assert!(verify(&h, "changeme"));
        let ms = t.elapsed().as_millis();
        eprintln!("PBKDF2 i={PBKDF2_ROUNDS} verify={ms}ms");
        // Deliberately a *ceiling against catastrophe*, not a benchmark: this
        // suite runs unoptimised and on shared CI hardware, so a tight bound
        // would be a flake generator.  Measured here at ~237 ms in release and
        // ~5.4 s in debug; 60 s catches only the case where someone has added
        // a couple of zeroes and made every login look like a hang.
        assert!(ms < 60_000, "a single login took {ms}ms -- is the round count sane?");
    }

    #[test]
    fn test_a_hashed_password_round_trips() {
        let h = cheap("correct horse battery staple");
        assert!(is_hashed(&h), "not a PHC string: {h}");
        assert!(h.starts_with("$pbkdf2-sha256$"), "expected PBKDF2-HMAC-SHA256, got {h}");
        assert!(verify(&h, "correct horse battery staple"));
        assert!(!verify(&h, "correct horse battery stapl"));
        assert!(!verify(&h, ""));
    }

    #[test]
    fn test_the_salt_is_random_so_two_hashes_of_one_password_differ() {
        // Not cosmetic: an unsalted scheme lets one cracked hash unlock every
        // installation that chose the same password, and lets an attacker
        // recognise a shared default across machines.
        let a = cheap("changeme");
        let b = cheap("changeme");
        assert_ne!(a, b, "two hashes of one password were identical -- no salt?");
        assert!(verify(&a, "changeme") && verify(&b, "changeme"));
    }

    #[test]
    fn test_cleartext_still_authenticates_so_an_upgrade_does_not_lock_anyone_out() {
        // Every release up to 1.0.0-RC1 stored cleartext.  Refusing it here
        // would lock existing installations out of their own gateway.
        assert!(verify("changeme", "changeme"));
        assert!(!verify("changeme", "wrong"));
        assert!(needs_rehash("changeme"));
        assert!(!needs_rehash(&cheap("changeme")));
    }

    #[test]
    fn test_an_empty_stored_credential_never_matches() {
        // `constant_time_eq(b"", b"")` is true, so this is the guard that
        // stops a blanked password becoming an accept-anything listener.
        assert!(!verify("", ""));
        assert!(!verify("", "anything"));
        assert!(!needs_rehash(""), "empty is 'unset', not 'needs rehashing'");
    }

    #[test]
    fn test_a_corrupt_hash_is_refused_not_treated_as_a_literal_password() {
        // If a truncated `$pbkdf2-...` value fell through to the cleartext
        // path, the file's own contents would become a working password.
        let broken = "$pbkdf2-sha256$i=210000,l=32$truncated";
        assert!(is_hashed(broken));
        assert!(!verify(broken, broken), "the hash text itself must not authenticate");
        assert!(!verify(broken, "changeme"));
    }

    #[test]
    fn test_a_hash_we_cannot_compute_is_refused_rather_than_compared_as_text() {
        // `is_hashed` tests the PHC *shape*, not our algorithm, and that
        // asymmetry is deliberate -- mistaking a hash for cleartext is the
        // dangerous direction.  So a well-formed hash from some other
        // algorithm reads as hashed, and `verify` must then refuse it rather
        // than fall back to comparing the text.  A real Argon2id string, from
        // the version of this feature that used it, is exactly that case.
        let argon = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$D3Fs+d1YrCJ8mZ9qEBBv+Q";
        assert!(is_hashed(argon), "a PHC string from another algorithm is still a hash");
        assert!(!needs_rehash(argon), "it is hashed, so it does not need rehashing");
        assert!(!verify(argon, argon), "the hash text itself must not authenticate");
        assert!(!verify(argon, "changeme"));
        // bcrypt's `$2b$` is PHC-shaped too, and gets the same treatment.
        assert!(is_hashed("$2b$12$abcdefghijklmnopqrstuv"));
        assert!(!verify("$2b$12$abcdefghijklmnopqrstuv", "changeme"));
        // ...while an ordinary password is not mistaken for any of it.
        assert!(!is_hashed("changeme"));
        assert!(!is_hashed("no$dollar$prefix"));
        assert!(!is_hashed("$"));
        assert!(!is_hashed("$$empty-algorithm"));
    }

    /// The cache is one global slot, so these must not race each other.
    static CACHE_TESTS: Mutex<()> = Mutex::new(());

    fn with_clean_cache<T>(f: impl FnOnce() -> T) -> T {
        let _g = CACHE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        *AUTH_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        f()
    }

    #[test]
    fn test_the_web_path_derives_once_and_then_rides_the_cache() {
        with_clean_cache(|| {
            let h = cheap("hunter2");
            let before = DERIVATIONS.load(Ordering::Relaxed);
            assert!(verify_cached(&h, "hunter2"));
            assert_eq!(DERIVATIONS.load(Ordering::Relaxed), before + 1, "first view must pay");
            // The polls: /vdm/frame every 150ms, /logs every 2s, and so on.
            for _ in 0..20 {
                assert!(verify_cached(&h, "hunter2"));
            }
            assert_eq!(
                DERIVATIONS.load(Ordering::Relaxed),
                before + 1,
                "a request behind the gate must not re-run the KDF"
            );
        });
    }

    #[test]
    fn test_changing_the_password_invalidates_the_cached_answer_at_once() {
        // This is what stands in for an expiry: there must be no window in
        // which the old password still opens the web UI.
        with_clean_cache(|| {
            let old = cheap("old-password");
            assert!(verify_cached(&old, "old-password"));
            let new = cheap("new-password");
            assert!(
                !verify_cached(&new, "old-password"),
                "the old password opened the UI after the password was changed"
            );
            assert!(verify_cached(&new, "new-password"));
        });
    }

    #[test]
    fn test_a_wrong_password_is_never_cached_and_always_pays() {
        // A cache that remembered failures would answer a near miss faster
        // than a distant one, and would make guessing cheap.
        with_clean_cache(|| {
            let h = cheap("hunter2");
            let before = DERIVATIONS.load(Ordering::Relaxed);
            for _ in 0..3 {
                assert!(!verify_cached(&h, "wrong"));
            }
            assert_eq!(
                DERIVATIONS.load(Ordering::Relaxed),
                before + 3,
                "every wrong guess must cost a full derivation"
            );
            // ...and a failure must not have poisoned the slot for the real one.
            assert!(verify_cached(&h, "hunter2"));
        });
    }

    #[test]
    fn test_cleartext_never_enters_the_cache() {
        with_clean_cache(|| {
            assert!(verify_cached("changeme", "changeme"));
            assert!(
                AUTH_CACHE.lock().unwrap().is_none(),
                "a cleartext password was remembered as a digest"
            );
            // And the empty-credential guard still holds on this path.
            assert!(!verify_cached("", ""));
        });
    }

    /// The threads a given supplied password was derived on.
    fn derived_on(supplied: &str) -> Vec<std::thread::ThreadId> {
        DERIVED_ON
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(s, _)| s == supplied)
            .map(|(_, t)| *t)
            .collect()
    }

    /// **The KDF leaves the runtime worker** -- for the per-session path
    /// (telnet, SSH) and for the web gate's cache miss alike.
    ///
    /// The runtime is current-thread, so the test's own thread *is* the only
    /// worker: a derivation recorded on it is one that blocked
    /// every task on the runtime. The thread is asserted rather than a
    /// stopwatch, because the cheap test hash is too fast to time and the
    /// production one too slow to wait for.
    #[test]
    fn test_the_kdf_runs_off_the_runtime_worker() {
        // Built here rather than by `#[tokio::test]` so the cache lock can be
        // held around the whole run without being held across an `.await`.
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        with_clean_cache(|| rt.block_on(kdf_off_worker_body()));
    }

    async fn kdf_off_worker_body() {
        let worker = std::thread::current().id();
        let h = cheap("off-worker-a");
        assert!(verify_off_runtime(&h, "off-worker-a").await);
        assert!(!verify_off_runtime(&h, "off-worker-wrong-a").await);
        for supplied in ["off-worker-a", "off-worker-wrong-a"] {
            let threads = derived_on(supplied);
            assert_eq!(threads.len(), 1, "{supplied}: expected exactly one derivation");
            assert_ne!(threads[0], worker, "{supplied}: the KDF ran on the runtime worker");
        }

        // The web gate: a miss derives off the worker, a hit derives nothing.
        let h = cheap("off-worker-b");
        assert!(verify_cached_off_runtime(&h, "off-worker-b").await);
        assert!(verify_cached_off_runtime(&h, "off-worker-b").await);
        assert!(!verify_cached_off_runtime(&h, "off-worker-wrong-b").await);
        assert!(!verify_cached_off_runtime(&h, "off-worker-wrong-b").await);
        let hits = derived_on("off-worker-b");
        assert_eq!(hits.len(), 1, "a cached success must not derive again");
        assert_ne!(hits[0], worker, "the web gate's miss ran the KDF on the worker");
        let misses = derived_on("off-worker-wrong-b");
        assert_eq!(misses.len(), 2, "a wrong password must pay every time");
        assert!(misses.iter().all(|t| *t != worker), "a wrong guess ran the KDF on the worker");
        // Cleartext and empty stay inline and keep their answers.
        assert!(verify_cached_off_runtime("changeme", "changeme").await);
        assert!(!verify_off_runtime("", "").await);
    }

    #[test]
    fn test_the_cache_key_binds_both_halves() {
        // Without the length prefix, ("ab","c") and ("a","bc") would collide,
        // which would let one credential answer for another.
        assert_ne!(cache_key("ab", "c"), cache_key("a", "bc"));
        assert_eq!(cache_key("a", "b"), cache_key("a", "b"));
        assert_ne!(cache_key("a", "b"), cache_key("a", "c"));
    }

    /// A typed password is hashed whatever it looks like, and the one that
    /// looks like a hash is the reason: `hash_if_cleartext` would leave it.
    /// Empty stays empty -- a hash of "" would accept an empty login.
    #[test]
    fn test_store_typed_hashes_whatever_was_typed() {
        let _rounds = CheapRounds::new();
        for typed in ["plain", "$ecret$2024", "$pbkdf2-sha256$not-really"] {
            let stored = store_typed(typed);
            assert!(verify(&stored, typed), "{typed:?} cannot log in");
            assert!(!verify(&stored, "wrong"));
        }
        let mut left = "$ecret$2024".to_string();
        hash_if_cleartext(&mut left);
        assert_eq!(left, "$ecret$2024", "the migration's rule changed; this test's premise is gone");
        assert_eq!(store_typed(""), "");
        // The one-key-at-a-time screens (telnet) route through this.
        assert!(verify(&typed_config_value("password", "$ecret$2024".into()), "$ecret$2024"));
        assert_eq!(typed_config_value("username", " ops ".into()), "ops", "a username is stored trimmed");
    }

    /// One username rule for every surface.  `:` is the one that matters most:
    /// HTTP Basic auth splits at the first colon, so `ops:pi` could log in to
    /// telnet and SSH and never to the web UI.
    #[test]
    fn test_a_username_any_surface_could_not_log_in_with_is_refused() {
        for bad in ["", "   ", "ops pi", "ops:pi", ":"] {
            assert!(username_problem(bad).is_some(), "{bad:?} was accepted");
        }
        for good in ["admin", "ops-pi", "ops.pi", " padded "] {
            assert_eq!(username_problem(good), None, "{good:?} was refused");
        }
    }
}
