# Changelog

## [Unreleased]

### Server

- Call hierarchy (`textDocument/prepareCallHierarchy`, `callHierarchy/incomingCalls`, `callHierarchy/outgoingCalls`) and type hierarchy (`textDocument/prepareTypeHierarchy`, `typeHierarchy/supertypes`, `typeHierarchy/subtypes`) for Python. Incoming calls of an Odoo model method include the calls on any record of the model (`self.method()`, `self.env['model'].method()`), from every override; the `super()` call of an override is not a caller. Subtypes include the Python subclasses and the classes that `_inherit` the model.
- These requests are served from a graph index of symbols and their uses, written during the initial build into a SQLite file in the user cache directory (`odoo-ls/graph/<pid>.sqlite`, rebuilt at every start, removed at exit; files left by a process that crashed are removed at the next start). Until the initial build is done they answer the error `odoo-ls: indexing in progress`; if the index fails, they answer `odoo-ls: graph index unavailable: <reason>` for the rest of the session. Files being edited are scanned on the fly until they are validated again.
- References to Python classes, functions and module- or class-level variables are read from the graph index once it is ready, which makes them faster on large projects, and also reach files the previous search did not scan. They now include calls through an imported name (`f()` after `from x import f`, where only the import line was reported) and hits in a package's `__init__.py` with their real range (instead of the package directory with an empty range). Without the index, references are searched as before.
- References honor `context.includeDeclaration`: the definition is added only when it is `true`, and on the range of its name instead of the whole `def` or `class` statement. Clients that map each location to its enclosing symbol no longer see every symbol as its own caller.
- The variables of a list, set or dict comprehension or of a generator expression are evaluated as the variable of a `for` loop: `rec` in `any(rec.is_open() for rec in records)` is a record of the model. Each comprehension has its own scope, as in Python 3, so its variables no longer resolve to an outer variable of the same name, and its iterables and conditions are now validated too (they were skipped, only the element was).
- The first parameter of a lambda given to `filtered`, `mapped`, `sorted` (or its `key`) and `grouped` of a recordset is a record of its model.
- `records | other`, `&`, `-` and `+` on a recordset give a recordset of the same model.
- A parameter annotated `Annotated[T, ...]` (from `typing` or `typing_extensions`) is evaluated as `T`: in a FastAPI route, `env: Annotated[Environment, Depends(...)]` is an `Environment` and `env["model"]` is the model.
- Lambdas and comprehensions in the value of an attribute (`records.filtered(lambda r: ...).mapped(...)`) and in the iterable of a `for` loop now get their symbols, so their parameters are known and the calls in their body are validated.
- `file_cache` now defaults to `false`. The AST and text of a Python file that is not opened are dropped once the file is built, and parsed again from disk when it is validated or when a feature needs it (the 64 most recent of these stay in memory). Ranges in a dropped file are converted without re-parsing it. Set `file_cache = true` to keep every AST in memory, as before.
- The background pre-parser parses at most 64 modules ahead of the build, and also parses the files ahead of their validation. Together with the dropped ASTs, this lowers the peak memory of the initial indexing of a ~2000-module project from 2.5 GB to 1.6 GB with `file_cache = false` (graph index included).
- Python files outside of the workspace (but not external) now drop their AST after validation too when `file_cache` is off.
- The symbols declared in a file, class or function body are kept in a flat map sorted by name and flow section, one 32-byte row per declaration, instead of a hash map of hash maps of vectors. The map and the section lists are shrunk at the end of ARCH_EVAL. Lookups are a binary search, and the completion prefix filter is a range scan. Scopes are now iterated in name order, so `import *` creates its variables in that order. The completion icon of an imported name comes from the declaration in the last section opened in the source. On a 2160-module tree, memory allocated after validation drops from 1110 MiB to 894 MiB, and from 2233 MiB to 2016 MiB with `file_cache`, with the same diagnostics and the same CPU time.
- With `file_cache = false`, the text of an XML, CSV or JS file that is not opened is now dropped too once the file is built: an XML file after its symbols are loaded, a CSV file after its validation, a JS file once its summary is built. It is read again from disk when a feature needs it, without parsing the JS file again, and positions in these files are converted without their text. On a ~2000-module project this frees about 115 MB of text for about 10 MB of line tables, and lowers the memory after validation from 1110 MiB to 992 MiB. A CSV file that cannot be read is now marked invalid instead of staying in validation. A file changed on disk while its text was dropped is reloaded once, and an offset that falls inside a character no longer panics.
- Diagnostics of a non-ASCII file whose text is in memory are converted to positions through a line table built for the publish, instead of counting the characters from the start of the line for each one. A minified JS bundle (one line of megabytes, thousands of diagnostics) took seconds: on a ~2000-module project with `file_cache = true` the initial indexing drops from about 58 s to 20 s, with the same diagnostics. A dropped text now also converts an offset inside a character to the position of that character, as the text does.
- A CSV file whose model has no class visible from its module no longer stays in validation: its validation is marked done and its previous CSV diagnostics are cleared.
- The fields of an XML or CSV record are kept in a `Vec` sorted by name, 24 bytes per field, instead of a hash map, and the text and `ref` of a field are stored as yarns (inline up to 15 bytes): a field symbol takes 96 bytes instead of 112. The fields of a record are now iterated in name order. On a ~2000-module project, memory allocated after validation drops from 775 MiB to 756 MiB, and from 2016 MiB to 1996 MiB with `file_cache`, with the same diagnostics.

## [1.6.0] - 2026/09/22 - JavaScript and performance

### Server

- Use OXC and tsserver to provide features for JavaScript files, as well as OWL templates.
- Provide semantic tokens for Python, XML and JS files.
- Store all symbols in an arena, and stop using `Rc<RefCell<>>` to store references.
    - Add GDB scripts to help debug this new memory management
- Switch the internal hasher from SipHash to FxHash for performance.
- Set `codegen-units` to `1` in release builds to improve runtime performance, at the cost of slower compilation.
- On Linux/macOS, switch the memory allocator to jemalloc.
- Support for the new `access` operator in search domains for Odoo >= 19.4
- Refactor the entire config parser to make adding new options easier in the future.
- Report loading progress as a percentage instead of number of items during initial loading.
- Lots of various optimizations.
- Improve unpacking evaluations, resolving code like `for a, b in [(1, 2), (2, 3)]`. `a` and `b` will now properly be evaluated as `int`
- Remove logs that were created on each string occurence of opened files.
- Configuration files that now contains invalid key or syntax errors will be reported in VsCode.
- Update gungraun to 0.19.4
- Various code style enhancements
- Handle special characters ' ', '"', '#', '%', '<', '>', '?', '[', ']', '^', '`', '{', '}', '|', '\\\\' in file URLs
- Semantic tokens can now be disabled in configuration with `disable_semantic_tokens_X = true`, with X in `[python, javascript, xml]`
- Fix and bring back OLS05001 in Xml fields references - `Unknown XML ID`
- Small refactoring of BuildSteps and SymbolKey replacement to improve stability by removing possible invalid states
- Windows builds now include line tables for better tracebacks in case of crash
- Add a warning diagnostic for calls to attributes that are not in dependencies
- GoTo features now select only the headers of classes and functions range. It allows us to bring back the default feature that call gotoreference instead of gotodefinition when clicking on the definition of the symbol
- Add features for `depends` field keyword argument to work in the same way as that for `api.depends` decorator arguments.
- Improve some feature preformances by not cloning the whole AST on requests, by introducing a lightweight enum `ASTKind`.
- More robust and extensive tests for JavaScript features and core.
- Correctly handle Javascript modules (correctly handle `@odoo-module` headers).
- Properly support assets at `.../static/tests` and `.../static/lib`.
- Implement completions for Javascript files.
- Properly store JS Symbols under a module's static dir as assets, and as children of the `ModuleSymbol` they are under.
- Improve and add new diagnostics for Javscript
- Add a new `ComponentManager` to manage the javascript components in a safe localized way
- Add features for newly added `compute_sql` field keyword argument to work like `compute` for odoo versions over `19.1`
- Add fearures for more Keyword/Positonal Arguments (`group_expand`, `selection` and `init_storage` (>= 20.0))


### VsCode

- Display configuration errors to draw user attention to invalid settings.
- Add a popup that strongly suggests disabling the built-in JS plugin in VS Code for your workspace. It will avoid having 2 instances of tsserver running serving the same answers to your requests.
- Notification that suggests disabling the built-in JavaScript plugin will now only be displayed if tsserver is configured and activated
- Remove the `editor.gotoLocation.alternativeDefinitionCommand` setting that was blocking GoToReference from Ctrl-Clicking a class name for example
- Only show Javascript messages if the server has typescript server running

### Fixes

- The server could sometimes get stuck in a state that consumed 100% CPU until the next request (typing, hover, etc.).
- Remove duplicate references found on the same line in XML files.
- Fix some missing references in the GoToReferences features (all references found in functions)
- Fix crash that occur on files that does not contain valid UTF-8
- Fix crash on invalid cycling evaluations.
- Fix Goto location when going on a python package
- Fix default config selection on non-odoo related workspace
- Fix Javascript internal dependency if the file is not part of the project
- Fix issues with compiled Python files.
- Warning about missing tsserver now suggests typescript@6 instead of typescript
- Fix diagnostic OLS05071 that was activating on t-name that contains a dot, which is valid
- Fix various hooks in ORM that could stop working after edits
- Fix cycle prevention in reference evaluation that was incorrectly preventing some valid results due to a different context.
- Fix crash on JavaScript file validation that can happen if AST is not ready or has been dropped
- Ensure that diagnostics are properly cleared when rebuilding an AST
- Fix useless custom entrypoint creation on files that were not opened by the user but modified on disk
- Fix internal file versioning for file info updates of opened files
- Fix range of hook on IrRule to apply only to Odoo < 19.4
- Fix panic on invalid syntax with missing type annotation in tuple assignment, like `a, b: int`
- Fix panic when calling super with an invalid argument like None

### Fixes not included in previous 1.5.2

- Fix an issue where builtins or other Python library files were dropped from the index if one of those files were opened and then closed
- Fix a bug where the workspace symbols were duplicated
- Fix the lifecycle of deleted addons, such that they can be recreated easily to avoid having odoo modules disappear forever
- Fix an issue where magic fields ("id", "display_name", ...) were added on every class in the model inheritence tree
- Fix an issue where completion items on class/model number could be duplicated due to multiple defintions
- Filter out magic fields on magic string features, for example trying to go to `"id"` field that just led to the class
- Fix the check for AST `is_built` for Javascript
- Fix the missing label details in completion for Javascript
- Fixes multiple JS missing or incorrect suggestiins in completions
- Fix the logical errors in the lifecycle of Javascript files, now CRUD is handled properly
- Fix a completion issue where `<lambda>` was shown in the list of member symbols
- Fix the look up of `check_js_symbol_path_vacant` to use `path` instead of `name`
- Fix a crash that happens when you request references on a relational field that is references in CSV without `/`or`:`
- Fix wrong semantic tokens on keys in the manifest
- Filter out virtual file artifacts from completions
- Fix shim paths being shown in import completion suggestions
