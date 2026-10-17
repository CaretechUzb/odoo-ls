# TODOS

## Memory

### References read dependent XML/CSV files from disk

**What:** Avoid reading, hashing and LRU-caching every dependent XML/CSV/JS file on each references request.

**Why:** With texts dropped, `features/references.rs` (~166/175/242/250) and `js_references.rs` (~159) read every dependent file from disk through `build_virtual_docs`/`read_real_js`, hash it and put it in the LRU. Past the 64-entry LRU (`REPARSED_AST_CAPACITY`), a big field-references request re-reads all of them every time. Correctness is fine; this is a latency item.

**Pros:** Faster field and JS references on large projects, with no change to the results.

**Cons:** A per-request text cache adds state to the request path; scanning without registering in the LRU needs a second read path.

**Context:** Options are a per-request text cache, or scanning without registering the files in the LRU. Measure a field-references request on the his tree before and after.

**Depends on / blocked by:** None.

### Scope map: long names are stored per row

**What:** Store a long name once per name instead of once per declaration in `ScopeMap`.

**Why:** `byteyarn` `Yarn::clone` deep-copies heap yarns longer than 15 bytes, so `ScopeMap::insert` (`server/src/core/symbols/storage/scope_map.rs`) stores a long name once per declaration, not once per name. The comment "reuse their yarn" there is misleading.

**Pros:** Less memory for scopes with many redeclared long names.

**Cons:** Needs a shared or interned name type, which touches every user of `OYarn`.

**Context:** Measure first, since the gain is unknown. Also consider `Yarn::copy(name)` instead of `oyarn!("{}", name)`.

**Depends on / blocked by:** None.
