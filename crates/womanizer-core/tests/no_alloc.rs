//! INFRA-03 violation test: the `assert_no_alloc` RT-safety guardrail fires on a heap
//! allocation inside a forbidden region in a debug build (Success Criterion #3).
//!
//! ## Why this test owns its own `#[global_allocator]`
//!
//! `assert_no_alloc` only detects allocations when its `AllocDisabler` is registered as the
//! process global allocator. The app crate registers it (Plan 04), but this is the
//! `womanizer-core` test binary — a separate process with the default system allocator. So we
//! register `AllocDisabler` here, gated to debug builds, exactly as the app does (D-11). In
//! release the guard compiles out (`disable_release`), so the whole test is `#[cfg(debug_assertions)]`.
//!
//! ## Why we read `violation_count()` instead of expecting `assert_no_alloc` to panic
//!
//! With the workspace's `warn_debug` feature (A1), an allocation inside `assert_no_alloc(|| ...)`
//! does NOT abort or panic — `AllocDisabler` increments a thread-local violation counter and
//! `assert_no_alloc` prints a warning, then returns normally (verified against
//! assert_no_alloc 1.1.2 source). To turn that detection into the catchable panic a
//! `#[should_panic]` test needs, we snapshot `violation_count()` around the region and panic
//! when it increased. The intentional `Box::new` is the sole cause of that increase; removing
//! it would leave the count unchanged and the `#[should_panic]` test would fail.

// Register the no-alloc allocator for THIS test binary, debug-only — mirrors the app crate.
#[cfg(debug_assertions)]
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

/// A heap allocation inside a forbidden region must trip the guardrail. The guard increments
/// the violation counter (warn_debug); we observe that and panic so `#[should_panic]` passes.
///
/// Both tests in this binary read and reset the process-global `assert_no_alloc` violation
/// counter. Cargo runs `#[test]`s in a binary on parallel threads by default — a reset in
/// one test can race the before/after snapshot in the other. The `serial_test` group name
/// serializes them against each other (and any future tests that touch the same counter)
/// without serializing unrelated tests.
#[cfg(debug_assertions)]
#[test]
#[serial_test::serial(no_alloc_violation_counter)]
#[should_panic(expected = "assert_no_alloc guard detected a forbidden allocation")]
fn no_alloc_guard_catches_violation() {
    assert_no_alloc::reset_violation_count();
    let before = assert_no_alloc::violation_count();

    assert_no_alloc::assert_no_alloc(|| {
        // Intentional heap allocation in an RT-forbidden region. This is what must trip the
        // guard; removing it leaves the count unchanged and fails the #[should_panic].
        let boxed = Box::new([0u8; 64]);
        std::hint::black_box(&boxed);
    });

    let after = assert_no_alloc::violation_count();
    if after > before {
        // Guard fired: convert the detected violation into the catchable panic that
        // #[should_panic(expected = ...)] observes. This is the success path.
        panic!("assert_no_alloc guard detected a forbidden allocation");
    }
    // No violation observed — the guard is not live (e.g. allocator not registered). Fail
    // with a DIFFERENT message so #[should_panic(expected = ...)] does NOT match it and the
    // test is correctly reported as failed.
    unreachable!("BUG: heap allocation was not detected by the assert_no_alloc guard");
}

/// Positive control: a copy-only region inside `assert_no_alloc` must NOT trip the guard.
/// Serialized against `no_alloc_guard_catches_violation` via the shared `serial_test` group
/// so a reset / intentional allocation in the other test cannot race this snapshot window.
#[cfg(debug_assertions)]
#[test]
#[serial_test::serial(no_alloc_violation_counter)]
fn no_alloc_guard_allows_copy_only_region() {
    assert_no_alloc::reset_violation_count();
    let before = assert_no_alloc::violation_count();

    let input = [0.25f32; 256];
    let mut out = [0f32; 256];
    assert_no_alloc::assert_no_alloc(|| {
        out.copy_from_slice(&input);
    });
    std::hint::black_box(&out);

    assert_eq!(
        assert_no_alloc::violation_count(),
        before,
        "copy-only region must not increment the allocation violation count"
    );
}
