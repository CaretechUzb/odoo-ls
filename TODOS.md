# TODOS

## Memory

### References read dependent XML/CSV files from disk

**What:** Avoid reading, hashing and LRU-caching every dependent XML/CSV/JS file on each references request.

**Why:** With texts dropped, `features/references.rs` (~166/175/242/250) and `js_references.rs` (~159) read every dependent file from disk through `build_virtual_docs`/`read_real_js`, hash it and put it in the LRU. Past the 64-entry LRU (`REPARSED_AST_CAPACITY`), a big field-references request re-reads all of them every time. Correctness is fine; this is a latency item.

**Pros:** Faster field and JS references on large projects, with no change to the results.

**Cons:** A per-request text cache adds state to the request path; scanning without registering in the LRU needs a second read path.

**Context:** Options are a per-request text cache, or scanning without registering the files in the LRU. Measure a field-references request on the his tree before and after.

**Depends on / blocked by:** None.

### Compact XML symbols

**What:** Shrink the XML record and field symbols: a flat sorted `Vec` for `XmlRecordSymbol.fields`, and `OYarn` instead of `String` for `XmlFieldSymbol.text` and `ref_key`.

**Why:** Measured on his, 2026-10-17: the `XmlRecordSymbol.fields` maps take 22.9 MiB, and a flat sorted `Vec` would cut about 12 MiB (estimated). `XmlFieldSymbol` is 112 B per slot over 524k slots, about 58 MiB (estimated). Field `text` and `ref_key` as `OYarn` would save about 8-12 MiB (estimated); field text totals 11.0 MiB measured over 436k texts.

**Pros:** About 30-80 MiB more off the memory after validation, without moving the symbol graph out of RAM.

**Cons:** Touches XML symbol lookup and every reader of the field text. Storing field text as source ranges was rejected: it saves at most about 11 MiB, while readers (`evaluation_utils.rs`, `completion.rs`, `xml_validation.rs` and others) read ir.model.fields records of OTHER files at request time. That would mean disk reads, XML entity/CDATA decoding and CSV quote decoding, and CSV offsets are not exact.

**Context:** Start from the `[mem]` lines and `metrics.rs` `log_symbol_counts`. Same regression contract as the other memory items (goldens, CLI `--parse` output digest, both `file_cache` modes).

**Depends on / blocked by:** None.

### Scope map: long names are stored per row

**What:** Store a long name once per name instead of once per declaration in `ScopeMap`.

**Why:** `byteyarn` `Yarn::clone` deep-copies heap yarns longer than 15 bytes, so `ScopeMap::insert` (`server/src/core/symbols/storage/scope_map.rs`) stores a long name once per declaration, not once per name. The comment "reuse their yarn" there is misleading.

**Pros:** Less memory for scopes with many redeclared long names.

**Cons:** Needs a shared or interned name type, which touches every user of `OYarn`.

**Context:** Measure first, since the gain is unknown. Also consider `Yarn::copy(name)` instead of `oyarn!("{}", name)`.

**Depends on / blocked by:** None.
