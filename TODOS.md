# TODOS

## Memory

### Scope map: long names are stored per row

**What:** Store a long name once per name instead of once per declaration in `ScopeMap`.

**Why:** `byteyarn` `Yarn::clone` deep-copies heap yarns longer than 15 bytes, so `ScopeMap::insert` (`server/src/core/symbols/storage/scope_map.rs`) stores a long name once per declaration, not once per name. The comment "reuse their yarn" there is misleading.

**Pros:** Less memory for scopes with many redeclared long names.

**Cons:** Needs a shared or interned name type, which touches every user of `OYarn`.

**Context:** Heap profile of feat/ram-next (his tree, 2026-10-17): all heap yarns come to about 24 MiB, ScopeMap rows to 38 MiB (32 bytes per row). Dropping the name from the rows and comparing with the symbol's own name would save about 19 MiB, but every step of the binary search would read a symbol: `ScopeMap::get` is hot in validation. Also consider `Yarn::copy(name)` instead of `oyarn!("{}", name)`.

**Depends on / blocked by:** None.

### Remaining holders after feat/ram-next

**What:** The largest holders at VALIDATION drained on the his tree (file_cache off, 471.7 MiB allocated), from the heap profile.

**Why:** Starting points for the next memory work.

**Context:**
- Symbol slot pages (`PagedSlotMap`), about 198 MiB: variables 61, XML fields 52, functions 26, classes 15. Moving the symbol table to SQLite was discussed and put aside.
- Evaluations, about 38 MiB; Evaluation Context, about 15 MiB.
- XML id tables (`insert_xml_id`, `XmlFieldMap`), about 20 MiB.
- FileInfo keeps its path twice (map key and `uri`), about 4 MiB.

**Depends on / blocked by:** None.
