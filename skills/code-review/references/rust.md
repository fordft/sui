# Rust review specifics

- `unwrap`/`expect` on fallible paths — is the invariant actually
  guaranteed, or just currently true?
- `unsafe`: what invariant does the safe wrapper restore? Check the
  contract comment exists and the boundary is minimal.
- Mutex/RwLock guard held across `.await` — !Send futures, deadlocks.
- `Arc` cycles, `clone()` in loops, unbounded `Vec`/`String` growth.
- Error handling: `?` propagation vs swallowed `let _ =` — a dropped
  Result is a finding, not a style nit.
- Iterator over-indexing: `.get()` vs `[i]` — panic paths on
  untrusted-length data.
- Lifetime extension via `to_string()`/`clone()` — hides ownership bugs?
- `impl Drop` side effects — do they run on panic paths too?
