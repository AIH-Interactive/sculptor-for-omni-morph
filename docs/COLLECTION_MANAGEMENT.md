# Collection management supplement

- `DELETE /api/library/v1/collection/:id` removes only the authenticated account's membership and outfit-candidate relationship. It is repeatable and can remove stale entries after model permissions are revoked.
- This endpoint does not delete the original model, downloaded files, another account's membership, or current equipment. The client uses the existing model deletion endpoint for its owner's own submissions.
- No schema or synchronization protocol changes are required. Existing authentication and UUID validation apply.
- Checkpoint (2026-10-20): `cargo test collection -- --nocapture` passed all four collection HTTP tests, including cross-account isolation, repeated deletion, inaccessible models, invalid IDs, original-model retention and persistence after restart.
