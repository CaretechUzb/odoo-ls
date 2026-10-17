# TODOS

## Memory

### Shrink scope maps and XML/CSV field text

**What:** Replace the per-scope `HashMap<OYarn, HashMap<u32, Vec<SymbolKey>>>` with a flat sorted `Vec` (name, offset, symbol), and store XML/CSV field text as source ranges instead of `String`.

**Why:** After the graph/RAM work these are the next likely RAM consumers: scope maps are estimated at ~100+ MB across ~450k names (200-250 B per name), and a profile showed `FileContentParent::add_child` at ~237 MB. Field text is read during validation (`xml_validation.rs`), so it stayed in RAM in M2 (D11); its size is not measured yet.

**Pros:** A further RAM cut without moving the hot symbol graph out of memory (D6 keeps `SymbolTable` in RAM).

**Cons:** Touches symbol lookup on the hot path (scope maps) and XML/CSV validation (field text); needs the same regression contract as the graph PR (goldens, CLI `--parse` output digest, both `file_cache` modes).

**Context:** Estimates from the 2026-10-16 eng review memory map. Start from the `[mem]` per-phase lines and `metrics.rs` `log_symbol_counts`, and measure before choosing the layout.

**Depends on / blocked by:** M0 per-phase memory logs (in place).

### Drop XML/JS `text_document` after the build

**What:** Drop the `text_document` of XML and JS files once their build steps are done, as M1 does for Python ASTs, and read the text again on demand.

**Why:** ~109 MB measured on the his tree: XML and JS sources stay in RAM for the whole session.

**Pros:** The largest measured RAM item left in `FileInfo`.

**Cons:** Blocked by `Ast::is_built()` always returning true for XML/CSV/JS (callers cannot tell that the text is gone) and by ~15 direct readers of `text_document` for these files; each needs a re-acquire path like `FileInfo::ensure_ast`.

**Context:** M1 dropped Python ASTs and kept XML/JS text (measured with the M0 `[mem]` lines). Survey the direct `text_document` readers first.

**Depends on / blocked by:** M0 per-phase memory logs (in place).
