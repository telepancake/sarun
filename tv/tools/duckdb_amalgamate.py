#!/usr/bin/env python3
"""Build a DuckDB amalgamation that bakes in the ``core_functions``
extension.

The vendored DuckDB amalgamation script (``deps/duckdb/scripts/amalgamation.py``)
only pulls in ``extension/loader/dummy_static_extension_loader.cpp`` — a no-op
that says "no extensions linked".  As a result the resulting libduckdb is
missing ``SUM``, ``AVG``, ``COUNT(DISTINCT)``, bitwise ``&``, ``regexp_*`` and
~hundreds more operators, all of which actually live in the optional but
near-mandatory ``core_functions`` extension.

This script monkey-patches the upstream ``amalgamation`` module to:

1. add ``extension/core_functions/include`` to the include search paths
2. add ``extension/core_functions`` to the compile directories so the .cpp
   files in it are concatenated into ``duckdb.cpp``
3. drop the dummy loader and emit a real ``LoadAllExtensions`` /
   ``LoadExtension`` / ``LoadedExtensionTestPaths`` triple that calls
   ``db.LoadStaticExtension<CoreFunctionsExtension>()``
4. add the extension's public header to ``duckdb.hpp``

The output goes to the same ``deps/duckdb/src/amalgamation`` directory, so
the existing Makefile rule that depends on ``$(DUCKDB_CPP)`` keeps working.
"""

from __future__ import annotations

import os
import sys


def main() -> int:
    repo_root = os.path.abspath(os.path.dirname(os.path.dirname(__file__)))
    duckdb_dir = os.path.join(repo_root, 'deps', 'duckdb')
    if not os.path.isdir(duckdb_dir):
        print(f'tools/duckdb_amalgamate.py: {duckdb_dir} missing — '
              f'run `git submodule update --init deps/duckdb` first',
              file=sys.stderr)
        return 1

    # The upstream amalgamation script does most of its work via module-level
    # globals (compile_directories, include_paths, main_header_files), then
    # finally calls generate_amalgamation().  We import it, patch the globals,
    # and call generate_amalgamation() ourselves.

    os.chdir(duckdb_dir)
    sys.path.insert(0, os.path.join(duckdb_dir, 'scripts'))

    import amalgamation  # type: ignore  # vendored

    ext_dir = os.path.join('extension', 'core_functions')
    ext_inc = os.path.join(ext_dir, 'include')
    if not os.path.isdir(ext_dir):
        print(f'tools/duckdb_amalgamate.py: {ext_dir} missing in submodule',
              file=sys.stderr)
        return 1

    # 1. include paths — prepend so the extension's headers shadow nothing
    amalgamation.include_paths = [ext_inc] + list(amalgamation.include_paths)

    # 2. compile directories — drop the dummy loader, add the extension and
    #    a generated real loader file (written into a temp location below).
    gen_loader_dir = os.path.join('extension', '_amalgam_loader')
    os.makedirs(gen_loader_dir, exist_ok=True)
    gen_loader_cpp = os.path.join(gen_loader_dir,
                                  'core_static_extension_loader.cpp')
    with open(gen_loader_cpp, 'w', encoding='utf8') as f:
        f.write(_LOADER_CPP)

    # Replace 'extension/loader' (dummy) with the new real loader directory,
    # add the core_functions tree.  The order matters: the libpg_query
    # grammar headers `#define INTEGER 474` (and ~hundred similar token
    # macros) and never #undef them, so any source file that mentions e.g.
    # `LogicalType::INTEGER` AFTER libpg_query in the amalgamation
    # macro-expands to `LogicalType::474` and fails to compile.  The
    # extension's source files do reference `LogicalType::INTEGER`, so we
    # must emit them BEFORE third_party — i.e. prepend, not append.
    cd = list(amalgamation.compile_directories)
    cd = [d for d in cd if os.path.normpath(d) != os.path.normpath(
              os.path.join('extension', 'loader'))]
    cd = [ext_dir, gen_loader_dir] + cd
    amalgamation.compile_directories = cd

    # 3. add the extension's public header to the umbrella so users (us) can
    #    `#include "duckdb.hpp"` and still get `CoreFunctionsExtension`.
    pub_hdr = os.path.join(ext_inc, 'core_functions_extension.hpp')
    amalgamation.main_header_files = list(amalgamation.main_header_files) + [pub_hdr]

    # Print what we're doing — this script is invoked from `make`, the user
    # might be wondering why this is taking 2 minutes.
    print('amalgamating duckdb with core_functions extension baked in:')
    print(f'  + include      {ext_inc}')
    print(f'  + sources      {ext_dir}')
    print(f'  + loader       {gen_loader_cpp} (replaces dummy)')
    print(f'  + header       {pub_hdr}')

    # Make sure the output directory exists (the upstream script assumes it
    # but doesn't create it on a fresh checkout).
    os.makedirs(os.path.dirname(amalgamation.source_file), exist_ok=True)

    amalgamation.generate_amalgamation(amalgamation.source_file,
                                       amalgamation.header_file)
    return 0


# A real ExtensionHelper::Load{All,}Extension implementation that links the
# core_functions extension statically.  Replaces the dummy loader.
_LOADER_CPP = '''\
// Auto-generated by tv/tools/duckdb_amalgamate.py — replaces the upstream
// extension/loader/dummy_static_extension_loader.cpp so that the resulting
// amalgamation actually exposes the operators that live in the
// core_functions extension (SUM, AVG, COUNT(DISTINCT), bitwise &, regexp_*,
// FIRST(...) ORDER BY ..., etc.).
#include "duckdb/main/extension_helper.hpp"
#include "core_functions_extension.hpp"

namespace duckdb {

ExtensionLoadResult ExtensionHelper::LoadExtension(DuckDB &db,
                                                   const std::string &extension) {
\tif (extension == "core_functions") {
\t\tdb.LoadStaticExtension<CoreFunctionsExtension>();
\t\treturn ExtensionLoadResult::LOADED_EXTENSION;
\t}
\treturn ExtensionLoadResult::NOT_LOADED;
}

void ExtensionHelper::LoadAllExtensions(DuckDB &db) {
\tLoadExtension(db, "core_functions");
}

vector<string> ExtensionHelper::LoadedExtensionTestPaths() {
\treturn {};
}

} // namespace duckdb
'''


if __name__ == '__main__':
    raise SystemExit(main())
