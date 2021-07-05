# Validation Code

Fetch both the validation code and the hash of the validation code used by a para, making the given
`OccupiedCoreAssumption`.

```rust
fn validation_code(at: Block, ParaId, OccupiedCoreAssumption) -> Option<ValidationCodeAndHash>;
```

NOTE: in API version 1, the returned type is `ValidationCode`, the hash is not returned.

Fetch the hash of the validation code used by a para, making the given `OccupiedCoreAssumption`.

```rust
fn validation_code_hash(at: Block, ParaId, OccupiedCoreAssumption) -> Option<ValidationCodeHash>;
```

Fetch the validation code (past, present or future) by its hash.

```rust
fn validation_code_by_hash(at: Block, ValidationCodeHash) -> Option<ValidationCode>;
```
