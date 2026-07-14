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
const SWIPL_TARGET: &str = "x86_64-linux-musl";

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

fn main() {
    println!("cargo:rerun-if-env-changed=SARUN_SWIPL_DIR");
    if env::var_os("CARGO_FEATURE_PROLOG").is_none() {
        return;
    }

    let target = env::var("TARGET").expect("Cargo did not set TARGET");
    assert_eq!(
        target, "x86_64-unknown-linux-musl",
        "the prolog feature currently supports only the static x86_64 musl build"
    );

    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let artifacts = env::var_os("SARUN_SWIPL_DIR").map_or_else(
        || {
            manifest
                .join("target/swipl")
                .join(SWIPL_VERSION)
                .join(SWIPL_TARGET)
        },
        PathBuf::from,
    );
    let info_path = artifacts.join("BUILD-INFO");
    let info = build_info(&info_path);
    require_value(&info, "pipeline", "4");
    require_value(&info, "target", SWIPL_TARGET);
    require_value(&info, "swipl_version", SWIPL_VERSION);
    require_value(&info, "swipl_commit", SWIPL_COMMIT);
    require_value(&info, "swipl_source_sha256", SWIPL_SOURCE_SHA256);
    require_value(&info, "zlib_commit", ZLIB_COMMIT);
    require_value(&info, "zlib_source_sha256", ZLIB_SOURCE_SHA256);
    require_value(&info, "use_signals", "OFF");

    let grammar = manifest.join("pl/action_grammar.pl");
    let swipl_license = manifest.join("../LICENSES/SWI-Prolog.txt");
    let zlib_license = manifest.join("../LICENSES/zlib.txt");
    require_value(&info, "action_grammar_sha256", &sha256(&grammar));
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
