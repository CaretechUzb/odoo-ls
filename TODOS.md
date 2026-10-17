# TODOS

## Memory

### Scope map: long names are stored per row

**What:** Store a long name once per name instead of once per declaration in `ScopeMap`.

**Why:** `byteyarn` `Yarn::clone` deep-copies heap yarns longer than 15 bytes, so `ScopeMap::insert` (`server/src/core/symbols/storage/scope_map.rs`) stores a long name once per declaration, not once per name. The comment "reuse their yarn" there is misleading.

**Pros:** Less memory for scopes with many redeclared long names.

**Cons:** Needs a shared or interned name type, which touches every user of `OYarn`.

**Context:** Measure first, since the gain is unknown. Also consider `Yarn::copy(name)` instead of `oyarn!("{}", name)`.

**Depends on / blocked by:** None.
