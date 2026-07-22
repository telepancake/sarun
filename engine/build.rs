use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SWIPL_VERSION: &str = "9.2.9";
const SWIPL_COMMIT: &str = "e3b19512e69a544f05b1bffbd14f3a0b519ad04d";
const SWIPL_SOURCE_SHA256: &str =
    "281e59fff098094bec8dc0831bd360c35d6360aaf12eebfc7b6be74f31d74d72";
const ZLIB_COMMIT: &str = "51b7f2abdade71cd9bb0e7a373ef2610ec6f9daf";
const ZLIB_SOURCE_SHA256: &str = "d9e270d46252734aa49770fbc544125391617956266f220bd63216c834f3a522";

fn require_file(path: &Path) {
    if !path.is_file() {
        panic!(
            "missing SWI-Prolog artifact {}; run `make swipl` first",
            path.display()
        );
    }
}

fn sha256(path: &Path) -> String {
    let output = Command::new("sha256sum")
        .arg("--binary")
        .arg(path)
        .output()
        .unwrap_or_else(|error| panic!("failed to run sha256sum for {}: {error}", path.display()));
    assert!(
        output.status.success(),
        "sha256sum failed for {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("sha256sum emitted non-UTF-8 output")
        .split_whitespace()
        .next()
        .expect("sha256sum emitted no digest")
        .to_owned()
}

fn build_info(path: &Path) -> BTreeMap<String, String> {
    require_file(path);
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
        .lines()
        .map(|line| {
            line.split_once('=')
                .unwrap_or_else(|| panic!("malformed BUILD-INFO line: {line}"))
        })
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn require_value(info: &BTreeMap<String, String>, key: &str, expected: &str) {
    assert_eq!(
        info.get(key).map(String::as_str),
        Some(expected),
        "stale SWI-Prolog BUILD-INFO {key}; run `make swipl`"
    );
}

fn require_generated_wire(manifest: &Path) {
    const PREFIX: &str = "// source-sha256 ";
    const SOURCES: &[&str] = &[
        "engine/pl/action_catalog.pl",
        "engine/pl/action_grammar.pl",
        "engine/pl/grammar_engine.pl",
        "engine/pl/text_grammar_engine.pl",
        "engine/pl/brush_grammar.pl",
        "engine/pl/evidence_projection.pl",
        "engine/pl/ast_state_relation.pl",
        "engine/pl/local_state_relation.pl",
        "engine/pl/grammar_codec.pl",
        "engine/pl/grammar_store.pl",
        "engine/pl/grammar_ir.pl",
        "engine/pl/relation_api.pl",
        "engine/pl/context_relation.pl",
        "engine/pl/transport_catalog.pl",
        "engine/pl/wire_codegen.pl",
        "scripts/wire_codegen.py",
    ];

    let generated = manifest.join("src/generated_wire.rs");
    require_file(&generated);
    let contents = fs::read_to_string(&generated)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", generated.display()));
    let recorded: BTreeMap<_, _> = contents
        .lines()
        .filter_map(|line| line.strip_prefix(PREFIX))
        .map(|line| {
            line.split_once(' ')
                .unwrap_or_else(|| panic!("malformed generated wire source hash: {line}"))
        })
        .collect();
    let expected_sources: std::collections::BTreeSet<_> = SOURCES.iter().copied().collect();
    let recorded_sources: std::collections::BTreeSet<_> = recorded.keys().copied().collect();
    assert_eq!(
        recorded_sources, expected_sources,
        "generated wire source set changed; run `make wire-codegen`"
    );

    let repository = manifest
        .parent()
        .expect("engine manifest must have a repository parent");
    for relative in SOURCES {
        let source = repository.join(relative);
        require_file(&source);
        let actual = sha256(&source);
        assert_eq!(
            recorded.get(relative).copied(),
            Some(actual.as_str()),
            "stale generated wire projection for {relative}; run `make wire-codegen`"
        );
        println!("cargo:rerun-if-changed={}", source.display());
    }
    println!("cargo:rerun-if-changed={}", generated.display());
}

fn main() {
    println!("cargo:rerun-if-env-changed=SARUN_SWIPL_DIR");
    let target = env::var("TARGET").expect("Cargo did not set TARGET");
    let swipl_target = match target.as_str() {
        "x86_64-unknown-linux-musl" => "x86_64-linux-musl",
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl",
        _ => {
            panic!("sarun's embedded Prolog runtime supports x86_64 and aarch64 musl, not {target}")
        }
    };

    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    require_generated_wire(&manifest);
    let artifacts = env::var_os("SARUN_SWIPL_DIR").map_or_else(
        || {
            manifest
                .join("target/swipl")
                .join(SWIPL_VERSION)
                .join(swipl_target)
        },
        PathBuf::from,
    );
    let info_path = artifacts.join("BUILD-INFO");
    let info = build_info(&info_path);
    require_value(&info, "pipeline", "5");
    require_value(&info, "target", swipl_target);
    require_value(&info, "swipl_version", SWIPL_VERSION);
    require_value(&info, "swipl_commit", SWIPL_COMMIT);
    require_value(&info, "swipl_source_sha256", SWIPL_SOURCE_SHA256);
    require_value(&info, "zlib_commit", ZLIB_COMMIT);
    require_value(&info, "zlib_source_sha256", ZLIB_SOURCE_SHA256);
    require_value(&info, "use_signals", "OFF");

    let grammar = manifest.join("pl/action_grammar.pl");
    let grammar_engine = manifest.join("pl/grammar_engine.pl");
    let text_grammar_engine = manifest.join("pl/text_grammar_engine.pl");
    let brush_grammar = manifest.join("pl/brush_grammar.pl");
    let evidence_projection = manifest.join("pl/evidence_projection.pl");
    let ast_state_relation = manifest.join("pl/ast_state_relation.pl");
    let local_state_relation = manifest.join("pl/local_state_relation.pl");
    let grammar_codec = manifest.join("pl/grammar_codec.pl");
    let grammar_store = manifest.join("pl/grammar_store.pl");
    let grammar_ir = manifest.join("pl/grammar_ir.pl");
    let relation_api = manifest.join("pl/relation_api.pl");
    let catalog = manifest.join("pl/action_catalog.pl");
    let context_relation = manifest.join("pl/context_relation.pl");
    let transport_catalog = manifest.join("pl/transport_catalog.pl");
    let swipl_license = manifest.join("../LICENSES/SWI-Prolog.txt");
    let zlib_license = manifest.join("../LICENSES/zlib.txt");
    require_value(&info, "action_grammar_sha256", &sha256(&grammar));
    require_value(&info, "grammar_engine_sha256", &sha256(&grammar_engine));
    require_value(
        &info,
        "text_grammar_engine_sha256",
        &sha256(&text_grammar_engine),
    );
    require_value(&info, "brush_grammar_sha256", &sha256(&brush_grammar));
    require_value(
        &info,
        "evidence_projection_sha256",
        &sha256(&evidence_projection),
    );
    require_value(
        &info,
        "ast_state_relation_sha256",
        &sha256(&ast_state_relation),
    );
    require_value(
        &info,
        "local_state_relation_sha256",
        &sha256(&local_state_relation),
    );
    require_value(&info, "grammar_codec_sha256", &sha256(&grammar_codec));
    require_value(&info, "grammar_store_sha256", &sha256(&grammar_store));
    require_value(&info, "grammar_ir_sha256", &sha256(&grammar_ir));
    require_value(&info, "relation_api_sha256", &sha256(&relation_api));
    require_value(&info, "action_catalog_sha256", &sha256(&catalog));
    require_value(&info, "context_relation_sha256", &sha256(&context_relation));
    require_value(
        &info,
        "transport_catalog_sha256",
        &sha256(&transport_catalog),
    );
    require_value(&info, "swipl_license_sha256", &sha256(&swipl_license));
    require_value(&info, "zlib_license_sha256", &sha256(&zlib_license));

    let required = [
        "boot.prc",
        "sarun.prc",
        "lib/libswipl.a",
        "lib/libz.a",
        "include/SWI-Prolog.h",
        "include/SWI-Stream.h",
        "include/zlib.h",
        "include/zconf.h",
        "LICENSES/SWI-Prolog.txt",
        "LICENSES/zlib.txt",
    ];
    for name in required {
        let path = artifacts.join(name);
        require_file(&path);
        require_value(&info, &format!("artifact.{name}.sha256"), &sha256(&path));
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-changed={}", info_path.display());
    println!("cargo:rerun-if-changed={}", grammar.display());
    println!("cargo:rerun-if-changed={}", grammar_engine.display());
    println!("cargo:rerun-if-changed={}", text_grammar_engine.display());
    println!("cargo:rerun-if-changed={}", brush_grammar.display());
    println!("cargo:rerun-if-changed={}", evidence_projection.display());
    println!("cargo:rerun-if-changed={}", ast_state_relation.display());
    println!("cargo:rerun-if-changed={}", local_state_relation.display());
    println!("cargo:rerun-if-changed={}", grammar_codec.display());
    println!("cargo:rerun-if-changed={}", grammar_store.display());
    println!("cargo:rerun-if-changed={}", grammar_ir.display());
    println!("cargo:rerun-if-changed={}", relation_api.display());
    println!("cargo:rerun-if-changed={}", catalog.display());
    println!("cargo:rerun-if-changed={}", context_relation.display());
    println!("cargo:rerun-if-changed={}", transport_catalog.display());
    println!("cargo:rerun-if-changed={}", swipl_license.display());
    println!("cargo:rerun-if-changed={}", zlib_license.display());

    let library_dir = artifacts.join("lib");
    println!("cargo:rustc-link-search=native={}", library_dir.display());
    println!("cargo:rustc-link-lib=static=swipl");
    println!("cargo:rustc-link-lib=static=z");
    println!("cargo:rustc-link-lib=m");
    println!("cargo:rustc-link-lib=rt");

    let embedded_resource = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("sarun.prc");
    fs::copy(artifacts.join("sarun.prc"), &embedded_resource).unwrap_or_else(|error| {
        panic!(
            "failed to copy embedded SWI resource to {}: {error}",
            embedded_resource.display()
        )
    });
    println!(
        "cargo:rustc-env=SARUN_SWIPL_RESOURCE={}",
        embedded_resource.display()
    );
}
