// `sarun oci load <ref> [NAME]` — populate a chain of at-rest sarun boxes
// from an OCI image, one box per layer. The bottom (rootfs) box is
// `--no-parent` (no_host_fallback=1) so the stack is closed; each layer
// above chains its `parent_box_id` to the previous layer's box. The image
// config (env / cmd / entrypoint / workingdir / user) is stored as JSON in
// the TOP layer box's sqlar meta.
//
// Box naming: the base box defaults to `C<box_id>`; layer boxes default to
// `L<box_id>`. A user-supplied NAME replaces the base box's name (layers
// keep `L<box_id>`), so `sarun oci load alpine:3.20 alpine` gives a stack
// you can address as `alpine`, `alpine.L<id>`, etc., via the normal dotted
// display path.
//
// Scope (v1): public registries (anonymous), local oci-archive: and
// oci-layout: refs; gzip + zstd + uncompressed layers; PAX/GNU long names;
// AUFS-style whiteouts — both per-sibling tombstones (`.wh.<name>`) and
// opaque-dir markers (`.wh..wh..opq`, ingested as sqlar.opaque=1 so the
// overlay hides every lower-layer entry under the dir — see
// test_oci_layers_rs.py); per-entry uid/gid/xattrs/mtime.
// Registry auth: credentials come from the host Docker config + credential
// helpers (registry_auth_for), read host-side, never entering a box. Key-based
// cosign signature verification is enforced when {config_home}/cosign.toml
// covers a reference (see oci_verify; keys read host-side).
// Out of scope (v1): keyless cosign (Fulcio/Rekor) verification; the
// zstd:chunked TOC fast path (plain-zstd decode is already correct).

use crate::depot::BoxDepot;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use oci_client::Reference;
use oci_client::client::{Client, ClientConfig, ImageLayer};
use oci_client::manifest::{
    IMAGE_LAYER_GZIP_MEDIA_TYPE, IMAGE_LAYER_MEDIA_TYPE,
};
use oci_client::secrets::RegistryAuth;

use crate::capture::BoxState;
use crate::paths;

const ZSTD_LAYER_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar+zstd";
const DOCKER_LAYER_GZIP_MEDIA_TYPE: &str =
    "application/vnd.docker.image.rootfs.diff.tar.gzip";

/// CLI dispatch: `sarun oci <subverb> <args...>`.
pub fn cli_oci(args: &[String]) -> i32 {
    let Some(sub) = args.first().map(String::as_str) else {
        eprintln!("usage: sarun oci <load|run|build|save|dockerfile|author> ...  (`sarun oci -h`)");
        return 2;
    };
    match sub {
        "load" => cli_load(&args[1..]),
        "run" => cli_run(&args[1..]),
        "build" => cli_build(&args[1..]),
        "save" => cli_save(&args[1..]),
        "dockerfile" => cli_dockerfile(&args[1..]),
        "author" => cli_author(&args[1..]),
        // Hidden: the host-side build worker the engine spawns for an in-box
        // `oci build` (see build_in_engine). Not for direct use.
        "__build-worker" => cli_build_worker(&args[1..]),
        "-h" | "--help" => {
            println!("usage:");
            println!("  sarun oci load <ref> [NAME]");
            println!("       populate at-rest boxes from an OCI image, one per layer");
            println!("  sarun oci run [--name NAME] [--net off|tap|host] <ref> [-- CMD...]");
            println!("       run a container on top of an image's box stack");
            println!("  sarun oci build [-t NAME] [-f FILE] [--net MODE] \
                      [--build-arg K=V]... [CONTEXT]");
            println!("       build an image box stack from a Dockerfile/Containerfile");
            println!("  sarun oci save <box> [-o FILE.tar]");
            println!("       export an image/container box stack to an oci-archive");
            println!("  sarun oci dockerfile <box>");
            println!("       print the Dockerfile that reconstructs a built/authored box");
            println!("  sarun oci author -t NAME --from BASE [--net MODE]");
            println!("       build an image interactively, one instruction per line");
            println!();
            println!("  ref  e.g. alpine:3.20, ghcr.io/foo/bar:tag,");
            println!("       oci-archive:/path/to.tar, oci-layout:/path/to/dir,");
            println!("       or the NAME/id of an already-loaded image box");
            0
        }
        other => {
            eprintln!("sarun oci: unknown subcommand '{other}' \
                       (try `sarun oci --help`)");
            2
        }
    }
}

// ── `sarun oci dockerfile` — reconstruct the recipe from a box's frames ──────
// Walk a built/authored box's chain: the prefix of boxes with no `frame` meta
// is the FROM'd base image; each box above carries the directives that built it
// (`frame.directives`, op+text). Print `FROM <base>` + the directive sequence.
fn cli_dockerfile(args: &[String]) -> i32 {
    let Some(boxname) = args.iter().find(|a| !a.starts_with('-')) else {
        eprintln!("usage: sarun oci dockerfile <box>");
        return 2;
    };
    match emit_dockerfile(boxname) {
        Ok(s) => { print!("{s}"); 0 }
        Err(e) => { eprintln!("sarun oci dockerfile: {e:#}"); 1 }
    }
}

fn emit_dockerfile(boxname: &str) -> Result<String> {
    let boxes = crate::discover::discover();
    let id = resolve_box(&boxes, boxname)
        .ok_or_else(|| anyhow!("no such box '{boxname}'"))?;
    let mut chain = vec![id];
    let mut cur = id;
    while let Some(p) = boxes.get(&cur).and_then(|b| b.parent) { chain.push(p); cur = p; }
    chain.reverse(); // base..top
    let has_frame = |b: &i64| boxes.get(b).is_some_and(|x| x.meta.contains_key("frame"));
    let first_build = chain.iter().position(has_frame);
    let (base, build): (&[i64], &[i64]) = match first_build {
        Some(i) => (&chain[..i], &chain[i..]),
        None => (&chain[..], &[]),
    };
    let mut out = String::new();
    if base.is_empty() {
        out.push_str("FROM scratch\n");
    } else {
        let bt = *base.last().unwrap();
        let bx = boxes.get(&bt);
        // Prefer a real registry reference; fall back to the box NAME (which
        // `oci build`'s FROM resolves) for archive/layout-loaded bases.
        let from = bx.and_then(|b| b.meta.get("oci_reference"))
            .filter(|r| !r.starts_with("oci-archive:") && !r.starts_with("oci-layout:"))
            .cloned()
            .or_else(|| bx.map(|b| b.name.clone()))
            .unwrap_or_else(|| bt.to_string());
        out.push_str(&format!("FROM {from}\n"));
    }
    for &bx in build {
        let Some(fr) = boxes.get(&bx).and_then(|b| b.meta.get("frame")) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(fr) else { continue };
        for d in v.get("directives").and_then(Value::as_array).into_iter().flatten() {
            let op = d.get("op").and_then(Value::as_str).unwrap_or("");
            let text = d.get("text").and_then(Value::as_str).unwrap_or("");
            if op.is_empty() { continue; }
            if text.is_empty() { out.push_str(op); }
            else { out.push_str(op); out.push(' '); out.push_str(text); }
            out.push('\n');
        }
        // Advisory: the longest common path this box's RUN wrote to. If those
        // files actually came from the host, the author can uncomment this and
        // fill in the source. A guess, not a directive — hence the comment.
        if let Some(h) = v.get("copy_hint").and_then(Value::as_str) {
            if !h.is_empty() {
                out.push_str(&format!("#COPY <host-source> {h}\n"));
            }
        }
    }
    Ok(out)
}

/// The `oci author` REPL's prompt: `author> ` (and `::: ` for continuation,
/// `(search)> ` while reverse-searching history).
struct AuthorPrompt;
impl reedline::Prompt for AuthorPrompt {
    fn render_prompt_left(&self) -> std::borrow::Cow<'_, str> { "author".into() }
    fn render_prompt_right(&self) -> std::borrow::Cow<'_, str> { "".into() }
    fn render_prompt_indicator(&self, _: reedline::PromptEditMode)
        -> std::borrow::Cow<'_, str> { "> ".into() }
    fn render_prompt_multiline_indicator(&self) -> std::borrow::Cow<'_, str> { "::: ".into() }
    fn render_prompt_history_search_indicator(&self, _: reedline::PromptHistorySearch)
        -> std::borrow::Cow<'_, str> { "(search)> ".into() }
}

/// Where the authoring REPL reads instruction lines from. At a tty we use
/// reedline (line editing + in-session history + reverse-search); piped or
/// redirected input falls back to plain stdin so scripts/tests are unchanged.
/// History is in-memory only — no file is written (authoring shouldn't leave a
/// shell-history trail, and the prototype keeps history off in this mode too).
enum AuthorLines {
    Tty(Box<reedline::Reedline>, AuthorPrompt),
    Pipe(std::io::Stdin, String),
}
impl AuthorLines {
    fn new() -> Self {
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() {
            AuthorLines::Tty(Box::new(reedline::Reedline::create()), AuthorPrompt)
        } else {
            AuthorLines::Pipe(std::io::stdin(), String::new())
        }
    }
    /// The next line, or None when the session should end (EOF / Ctrl-D). A
    /// Ctrl-C at the tty cancels the current line (returns an empty string the
    /// caller skips) rather than quitting.
    fn next_line(&mut self) -> Option<String> {
        match self {
            AuthorLines::Tty(ed, prompt) => match ed.read_line(prompt) {
                Ok(reedline::Signal::Success(s)) => Some(s),
                Ok(reedline::Signal::CtrlC) => Some(String::new()),
                Ok(reedline::Signal::CtrlD) => None,
                Ok(_) => None,   // Signal is #[non_exhaustive]
                Err(_) => None,
            },
            AuthorLines::Pipe(stdin, buf) => {
                buf.clear();
                match stdin.read_line(buf) {
                    Ok(0) => None,
                    Ok(_) => Some(std::mem::take(buf)),
                    Err(_) => None,
                }
            }
        }
    }
}

// ── `sarun oci author` — build an image interactively, one instruction/line ──
// An interactive Dockerfile builder: each submitted line is one instruction run
// through the same Builder `oci build` uses, so it creates the layer box + the
// per-box `frame` + history. Bare `cd`/`export` persist as WORKDIR/ENV; any
// other bare line is a RUN; explicit Dockerfile keywords are parsed as such.
// `undo` discards the box(es) the last line created and rolls back state;
// `done`/EOF finalizes the image and prints its Dockerfile. At a tty it reads
// through reedline (line editing + history + reverse-search); piped input uses
// plain stdin, so scripts behave identically. RUN executes in a box, so a live
// engine is required — run on the host.
fn cli_author(args: &[String]) -> i32 {
    let mut tag: Option<String> = None;
    let mut from: Option<String> = None;
    let mut net_mode = crate::net::NetMode::Tap;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-t" | "--tag" => tag = it.next().cloned(),
            "--from" => from = it.next().cloned(),
            "--net" => match it.next().map(String::as_str).and_then(crate::net::NetMode::parse) {
                Some(nm) => net_mode = nm,
                None => { eprintln!("sarun oci author: --net wants off|tap|host"); return 2; }
            },
            "-h" | "--help" => {
                println!("usage: sarun oci author -t NAME --from BASE [--net MODE]");
                println!("  then, one instruction per line on stdin:");
                println!("    <cmd>            → RUN <cmd>   (bare command)");
                println!("    cd DIR           → WORKDIR DIR (persists)");
                println!("    export K=V       → ENV K=V     (persists)");
                println!("    RUN/COPY/ENV/…   → that Dockerfile instruction");
                println!("    undo             → drop the last instruction + its layer");
                println!("    print            → show the Dockerfile so far");
                println!("    done             → finalize the image (EOF also works)");
                return 0;
            }
            other => { eprintln!("sarun oci author: unexpected argument '{other}'"); return 2; }
        }
    }
    let (Some(tag), Some(from)) = (tag, from) else {
        eprintln!("usage: sarun oci author -t NAME --from BASE [--net MODE]");
        return 2;
    };
    if in_box() {
        eprintln!("sarun oci author: run on the host (RUN executes in a box and \
                   needs the engine)");
        return 1;
    }
    if let Err(e) = paths::ensure_dirs() { eprintln!("sarun oci author: {e}"); return 1; }
    let mut b = Builder::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                             net_mode, Vec::new());
    if let Err(e) = b.do_from(&from, &None) {
        eprintln!("sarun oci author: FROM {from}: {e:#}");
        return 1;
    }
    eprintln!("authoring '{tag}' FROM {from} — one instruction per line; \
               `undo`, `print`, `done`.");
    let mut lines = AuthorLines::new();
    let mut undo: Vec<(Builder, String)> = Vec::new();
    loop {
        let Some(line) = lines.next_line() else { break };   // EOF / Ctrl-D = done
        let t = line.trim().to_string();
        if t.is_empty() { continue; }
        match t.as_str() {
            "done" | "save" => break,
            "print" | "dockerfile" => {
                b.stamp_frames();
                match emit_dockerfile(&b.current.map(|c| c.to_string()).unwrap_or_default()) {
                    Ok(df) => print!("{df}"),
                    Err(e) => eprintln!("(print: {e})"),
                }
            }
            "undo" => {
                if let Some((snap, undone)) = undo.pop() {
                    // Delete any box the undone instruction created (a RUN/COPY
                    // layer); a config-only instruction created none.
                    let new_boxes: Vec<i64> = b.frames.keys()
                        .filter(|k| !snap.frames.contains_key(k)).copied().collect();
                    for id in new_boxes { delete_box(id); }
                    b = snap;
                    eprintln!("undone: {undone}");
                } else {
                    eprintln!("nothing to undo");
                }
            }
            _ => {
                let snap = b.clone();
                let instr = author_line_to_instruction(&t);
                match b.exec(&instr) {
                    Ok(()) => {
                        // A RUN that wrote files gets an advisory COPY hint: the
                        // longest common path of this box's own writes. Cheap
                        // (one pass over the new layer); skipped when scattered.
                        if matches!(instr, Instruction::Run(_)) {
                            if let Some(id) = b.current {
                                let h = box_write_lcp(id);
                                if !h.is_empty() && h != "/" {
                                    b.copy_hints.insert(id, h);
                                }
                            }
                        }
                        undo.push((snap, t.clone()));
                    }
                    Err(e) => { eprintln!("error: {e:#}"); b = snap; }
                }
            }
        }
    }
    match b.finish(Some(tag.clone())) {
        Ok(()) => {
            match emit_dockerfile(&tag) {
                Ok(df) => { println!("--- Dockerfile ---\n{df}"); 0 }
                Err(_) => 0,
            }
        }
        Err(e) => { eprintln!("sarun oci author: {e:#}"); 1 }
    }
}

/// Map one authored prompt line to a Dockerfile instruction: bare `cd`/`export`
/// persist as WORKDIR/ENV; an explicit Dockerfile keyword is parsed as such;
/// anything else is a shell-form RUN of the whole line.
fn author_line_to_instruction(t: &str) -> Instruction {
    if let Some(p) = t.strip_prefix("cd ") {
        return Instruction::Workdir(p.trim().to_string());
    }
    if let Some(kv) = t.strip_prefix("export ") {
        if let Some((k, v)) = kv.trim().split_once('=') {
            return Instruction::Env(vec![(k.trim().to_string(), v.trim().to_string())]);
        }
    }
    const KW: &[&str] = &["RUN", "COPY", "ADD", "ENV", "WORKDIR", "USER", "CMD",
        "ENTRYPOINT", "LABEL", "EXPOSE", "VOLUME", "SHELL", "STOPSIGNAL", "ARG",
        "ONBUILD", "HEALTHCHECK"];
    let kw = t.split_whitespace().next().unwrap_or("").to_uppercase();
    if KW.contains(&kw.as_str()) {
        if let Ok(df) = crate::dockerfile::Dockerfile::parse(t) {
            if let Some((_, instr)) = df.instructions.into_iter().next() {
                return instr;
            }
        }
    }
    Instruction::Run(Cmdline::Shell(t.to_string()))
}

/// Delete an at-rest box created during authoring (undo). Prefer the engine's
/// reaper; fall back to removing its on-disk state.
fn delete_box(id: i64) {
    if let Some(conn) = engine_conn() {
        if engine_rpc_on(conn, "delete", serde_json::json!([id.to_string()])).is_ok() {
            return;
        }
    }
    let _ = std::fs::remove_file(paths::state_home().join(format!("{id}.sqlar")));
    let _ = std::fs::remove_dir_all(paths::state_home().join("blob").join(id.to_string()));
    let _ = std::fs::remove_dir_all(paths::live_home().join(id.to_string()));
}

// ── `sarun oci save` — the inverse of `oci load` ─────────────────────────────
// Export a box (an OCI image/container stack from load/build/run) back to an
// oci-archive: tar of an oci-layout, consumable by `oci load oci-archive:` and
// by skopeo. Faithful: each box in the parent chain becomes one gzip tar layer
// (inverting ingest_layer — files/dirs/symlinks/whiteouts/opaque/devices +
// ownership), and the image config carries forward (rootfs.diff_ids rewritten
// to the re-emitted layers). Host-side, read-only over at-rest box sqlars. The
// chain must bottom at a CLOSED base (no_host_fallback) — a host-lower box's
// diff isn't a standalone image.
fn cli_save(args: &[String]) -> i32 {
    let mut boxname: Option<String> = None;
    let mut dest: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" | "--output" => match it.next() {
                Some(v) => dest = Some(v.clone()),
                None => { eprintln!("sarun oci save: {a} needs an argument"); return 2; }
            },
            "-h" | "--help" => {
                println!("usage: sarun oci save <box> [-o FILE.tar]");
                return 0;
            }
            other if boxname.is_none() => boxname = Some(other.to_string()),
            other => { eprintln!("sarun oci save: unexpected argument '{other}'"); return 2; }
        }
    }
    let Some(boxname) = boxname else {
        eprintln!("usage: sarun oci save <box> [-o FILE.tar]");
        return 2;
    };
    if in_box() {
        eprintln!("sarun oci save: run on the host — it reads at-rest box state, \
                   which a closed box can't see");
        return 1;
    }
    match save_box(&boxname, dest.as_deref()) {
        Ok((path, n)) => {
            println!("saved box '{boxname}' → oci-archive '{path}' ({n} layer(s))");
            0
        }
        Err(e) => { eprintln!("sarun oci save: {e:#}"); 1 }
    }
}

fn save_box(boxname: &str, dest: Option<&str>) -> Result<(String, usize)> {
    let boxes = crate::discover::discover();
    let id = resolve_box(&boxes, boxname)
        .ok_or_else(|| anyhow!("no such box '{boxname}'"))?;
    // Walk the parent chain to the root, then reverse → base..top (= layer order).
    let mut chain = vec![id];
    let mut cur = id;
    while let Some(p) = boxes.get(&cur).and_then(|b| b.parent) {
        chain.push(p);
        cur = p;
    }
    chain.reverse();
    let base = chain[0];
    if boxes.get(&base).and_then(|b| b.meta.get("no_host_fallback")).map(String::as_str)
        != Some("1") {
        bail!("box '{boxname}' is not a closed image stack (its base falls through \
               to the host) — `oci save` needs a load/build/run image base");
    }
    // The image config: the highest oci_config in the chain (top→base).
    let cfg_json = chain.iter().rev()
        .find_map(|b| boxes.get(b).and_then(|x| x.meta.get("oci_config")).cloned())
        .ok_or_else(|| anyhow!("box '{boxname}' carries no oci_config — not an OCI \
                                image (load/build it first)"))?;

    let layout = paths::runtime_home()
        .join(format!("oci-save-{}-{}", std::process::id(), now_ns()));
    let blobs = layout.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs).context("create save layout")?;
    let _guard = TmpCleanup { dir: layout.clone(), file: layout.join(".none") };

    let mut diff_ids: Vec<String> = Vec::new();
    let mut layer_descs: Vec<Value> = Vec::new();
    for &bx in &chain {
        let tar = build_layer_tar(bx)
            .with_context(|| format!("export layer box {bx}"))?;
        diff_ids.push(format!("sha256:{}", sha256_hex(&tar)));
        let mut enc = flate2::write::GzEncoder::new(Vec::new(),
                                                    flate2::Compression::default());
        std::io::Write::write_all(&mut enc, &tar).context("gzip layer")?;
        let gz = enc.finish().context("finish gzip layer")?;
        let ldg = format!("sha256:{}", sha256_hex(&gz));
        std::fs::write(blobs.join(ldg.trim_start_matches("sha256:")), &gz)
            .context("write layer blob")?;
        layer_descs.push(serde_json::json!({
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": ldg, "size": gz.len(),
        }));
    }

    // Config: carry the image config forward, rewriting rootfs.diff_ids to the
    // re-emitted layers and ensuring architecture/os are present.
    let mut cfg: Value = serde_json::from_str(&cfg_json).context("parse oci_config")?;
    if let Value::Object(ref mut m) = cfg {
        m.insert("rootfs".into(), serde_json::json!({
            "type": "layers", "diff_ids": diff_ids.clone() }));
        m.entry("architecture").or_insert_with(|| Value::String("amd64".into()));
        m.entry("os").or_insert_with(|| Value::String("linux".into()));
        // Preserve the build/load history (created_by recipe). It carries no
        // layer digests, only an ordered empty_layer flag sequence, so it stays
        // valid across re-emission AS LONG AS its non-empty count matches the
        // layer count. Pad with generic entries when a box on top (e.g. a
        // modified container) added layers the history doesn't mention.
        let hist = m.remove("history").and_then(|h| match h {
            Value::Array(a) => Some(a), _ => None }).unwrap_or_default();
        let non_empty = hist.iter()
            .filter(|e| e.get("empty_layer").and_then(Value::as_bool) != Some(true))
            .count();
        let mut hist = hist;
        for _ in non_empty..diff_ids.len() {
            hist.push(serde_json::json!({
                "created_by": "sarun: box changes", "empty_layer": false }));
        }
        if !hist.is_empty() { m.insert("history".into(), Value::Array(hist)); }
    }
    let cfg_b = serde_json::to_vec(&cfg).context("serialize config")?;
    let cfg_d = format!("sha256:{}", sha256_hex(&cfg_b));
    std::fs::write(blobs.join(cfg_d.trim_start_matches("sha256:")), &cfg_b)
        .context("write config blob")?;

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                   "digest": cfg_d, "size": cfg_b.len()},
        "layers": layer_descs,
    });
    let man_b = serde_json::to_vec(&manifest).context("serialize manifest")?;
    let man_d = format!("sha256:{}", sha256_hex(&man_b));
    std::fs::write(blobs.join(man_d.trim_start_matches("sha256:")), &man_b)
        .context("write manifest blob")?;

    let index = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": man_d, "size": man_b.len(),
            "annotations": {"org.opencontainers.image.ref.name": boxname},
        }],
    });
    std::fs::write(layout.join("index.json"),
                   serde_json::to_vec(&index).context("serialize index")?)
        .context("write index.json")?;
    std::fs::write(layout.join("oci-layout"),
                   serde_json::json!({"imageLayoutVersion": "1.0.0"}).to_string())
        .context("write oci-layout")?;

    let dest = dest.map(String::from).unwrap_or_else(|| format!("{boxname}.tar"));
    _tar_layout_to(&layout, Path::new(&dest)).context("write oci-archive")?;
    Ok((dest, chain.len()))
}

/// Build the uncompressed tar of one box's layer (its sqlar diff), inverting
/// `ingest_layer`: regular files (bytes from the blob pool), dirs (+ a
/// `.wh..wh..opq` when opaque), symlinks, char/block/fifo devices, and S_IFCHR
/// whiteout tombstones (→ `.wh.<name>`). Ownership rides along.
fn build_layer_tar(box_id: i64) -> Result<Vec<u8>> {
    let sqlar = paths::state_home().join(format!("{box_id}.sqlar"));
    let conn = rusqlite::Connection::open_with_flags(
        &sqlar, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {}", sqlar.display()))?;
    let mut tb = tar::Builder::new(Vec::new());
    let rows = crate::depot::archive_all_nodes(&conn)?;
    for (rowid, name, mode, data, opaque) in rows {
        let kind = mode & 0o170000;
        let perm = mode & 0o7777;
        let (uid, gid) = owner_lookup(&conn, &name);
        if name.is_empty() {
            // The box-root opaque marker (name="") — emit it at the tar root.
            if opaque != 0 { append_empty_file(&mut tb, ".wh..wh..opq")?; }
            continue;
        }
        match kind {
            0o040000 => {   // directory
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(tar::EntryType::Directory);
                h.set_mode(perm); h.set_size(0);
                h.set_uid(uid as u64); h.set_gid(gid as u64);
                tb.append_data(&mut h, &name, std::io::empty())?;
                if opaque != 0 {
                    append_empty_file(&mut tb, &format!("{name}/.wh..wh..opq"))?;
                }
            }
            0o120000 => {   // symlink — target is the sqlar data
                let target = String::from_utf8_lossy(&data.unwrap_or_default())
                    .into_owned();
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(tar::EntryType::Symlink);
                h.set_mode(perm); h.set_size(0);
                h.set_uid(uid as u64); h.set_gid(gid as u64);
                tb.append_link(&mut h, &name, &target)?;
            }
            0o020000 if rdev_lookup(&conn, &name).is_none() => {
                // S_IFCHR with no device row = a whiteout tombstone → `.wh.<name>`.
                append_empty_file(&mut tb, &whiteout_path(&name))?;
            }
            0o020000 | 0o060000 | 0o010000 => {   // char / block / fifo device
                let dev = rdev_lookup(&conn, &name).unwrap_or(0);
                let et = match kind {
                    0o020000 => tar::EntryType::Char,
                    0o060000 => tar::EntryType::Block,
                    _ => tar::EntryType::Fifo,
                };
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(et);
                h.set_mode(perm); h.set_size(0);
                h.set_uid(uid as u64); h.set_gid(gid as u64);
                let _ = h.set_device_major(((dev >> 8) & 0xfff) as u32);
                let _ = h.set_device_minor((dev & 0xff) as u32);
                tb.append_data(&mut h, &name, std::io::empty())?;
            }
            _ => {   // regular file — bytes live in the blob pool
                let bytes = std::fs::read(crate::depot::blob_path(box_id, rowid))
                    .unwrap_or_default();
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(perm); h.set_size(bytes.len() as u64);
                h.set_uid(uid as u64); h.set_gid(gid as u64);
                tb.append_data(&mut h, &name, &bytes[..])?;
            }
        }
    }
    tb.into_inner().context("finish layer tar")
}

/// `.wh.<basename>` in the entry's parent directory (AUFS whiteout convention).
fn whiteout_path(name: &str) -> String {
    match name.rsplit_once('/') {
        Some((parent, base)) => format!("{parent}/.wh.{base}"),
        None => format!(".wh.{name}"),
    }
}

fn append_empty_file<W: std::io::Write>(tb: &mut tar::Builder<W>, path: &str)
    -> Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Regular);
    h.set_mode(0o644); h.set_size(0);
    tb.append_data(&mut h, path, std::io::empty())?;
    Ok(())
}

fn owner_lookup(conn: &rusqlite::Connection, name: &str) -> (u32, u32) {
    conn.query_row("SELECT uid,gid FROM ownership WHERE name=?1", [name],
                   |r| Ok((r.get::<_, i64>(0)? as u32, r.get::<_, i64>(1)? as u32)))
        .unwrap_or((0, 0))
}

fn rdev_lookup(conn: &rusqlite::Connection, name: &str) -> Option<u64> {
    conn.query_row("SELECT dev FROM rdev WHERE name=?1", [name],
                   |r| r.get::<_, i64>(0)).ok().map(|d| d as u64)
}

/// Component-wise longest common prefix of a set of paths. A single path is its
/// own LCP (so a one-file write yields that exact file); fully divergent paths
/// yield "" (the worst case the caller treats as "no useful hint").
fn paths_lcp(paths: &[String]) -> String {
    let mut it = paths.iter();
    let Some(first) = it.next() else { return String::new() };
    let mut prefix: Vec<&str> = first.split('/').collect();
    for p in it {
        let comps: Vec<&str> = p.split('/').collect();
        let n = prefix.iter().zip(comps.iter()).take_while(|(a, b)| a == b).count();
        prefix.truncate(n);
        if prefix.is_empty() { break; }
    }
    prefix.join("/")
}

/// The longest common path of a box's own writes — one read-only pass over its
/// layer (cheap). Directories, whiteout tombstones, and the root opaque marker
/// don't count as writes (a `mkdir -p` shouldn't drag the prefix up a level and
/// spoil the single-file case). Returns an absolute path, or "" when there are
/// no file writes or they're too scattered to share a prefix.
fn box_write_lcp(box_id: i64) -> String {
    let sqlar = paths::state_home().join(format!("{box_id}.sqlar"));
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &sqlar, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else { return String::new() };
    let mut names = Vec::new();
    for (name, mode) in crate::depot::archive_names_modes(&conn) {
        if name.is_empty() { continue; }
        let kind = mode & 0o170000;
        if kind == 0o040000 { continue; }                                   // dir
        if kind == 0o020000 && rdev_lookup(&conn, &name).is_none() { continue; } // whiteout
        names.push(name);
    }
    if names.is_empty() { return String::new(); }
    let lcp = paths_lcp(&names);
    if lcp.is_empty() { String::new() } else { format!("/{lcp}") }
}

/// Tar an oci-layout dir into `dest` (oci-archive format).
fn _tar_layout_to(layout: &Path, dest: &Path) -> Result<()> {
    let f = File::create(dest)
        .with_context(|| format!("create {}", dest.display()))?;
    let mut tb = tar::Builder::new(f);
    tb.append_path_with_name(layout.join("oci-layout"), "oci-layout")?;
    tb.append_path_with_name(layout.join("index.json"), "index.json")?;
    tb.append_dir_all("blobs", layout.join("blobs"))?;
    tb.finish()?;
    Ok(())
}

fn cli_load(args: &[String]) -> i32 {
    let Some(reference) = args.first().cloned() else {
        eprintln!("usage: sarun oci load <ref> [NAME]");
        return 2;
    };
    let name = args.get(1).cloned();
    // The pull runs in the ENGINE (host-side): credentials stay out of any box,
    // and an in-box `sarun oci load` never unpacks through the box's netns+FUSE.
    // Fall back to a local pull only when no engine is reachable (host, no
    // `serve`); an in-box caller must use the engine (broker), never local.
    match engine_conn() {
        Some(conn) => {
            match engine_rpc_on(conn, "oci.load",
                                serde_json::json!([reference, name])) {
                Ok(r) => {
                    if r.get("verified").and_then(Value::as_bool) == Some(true) {
                        eprintln!("sarun oci: cosign signature verified for \
                                   '{reference}'");
                    }
                    println!("loaded image '{reference}': base box '{}' (id={}) → \
                              top box '{}' (id={}), {} layer(s)",
                        r.get("base_name").and_then(Value::as_str).unwrap_or("?"),
                        r.get("base_id").and_then(Value::as_i64).unwrap_or(0),
                        r.get("top_name").and_then(Value::as_str).unwrap_or("?"),
                        r.get("top_id").and_then(Value::as_i64).unwrap_or(0),
                        r.get("n_layers").and_then(Value::as_i64).unwrap_or(0));
                    return 0;
                }
                Err(e) => { eprintln!("sarun oci load: {e}"); return 1; }
            }
        }
        None if in_box() => {
            eprintln!("sarun oci load: in a box but the engine broker is \
                       unreachable — cannot pull");
            return 1;
        }
        None => {}  // host, no engine: fall through to a local pull
    }
    if let Err(e) = paths::ensure_dirs() {
        eprintln!("sarun oci load: {e}");
        return 1;
    }
    match load_blocking(&reference, name) {
        Ok(r) => {
            if r.verified {
                eprintln!("sarun oci: cosign signature verified for '{reference}'");
            }
            // Spell out both ends of the chain so the user can address the
            // image as either `<base>` (full merged view) or `<base>.L<n>`
            // (a specific layer). Single-layer images have base == top.
            println!("loaded image '{reference}': base box '{}' (id={}) → \
                      top box '{}' (id={}), {} layer(s)",
                     r.base_name, r.base_id, r.top_name, r.top_id, r.n_layers);
            0
        }
        Err(e) => {
            eprintln!("sarun oci load: {e:#}");
            1
        }
    }
}

// ── `sarun oci run` ──────────────────────────────────────────────────────────
// Run a container on top of an OCI image's box stack. The image's TOP layer box
// becomes the parent of a fresh, ephemeral container box: the engine walks that
// parent chain, finds the `oci_config` meta the loader stamped, and fills in
// env / workdir / user / entrypoint+cmd in the register ack — so an empty CMD
// runs the image's own entrypoint, and a supplied CMD overrides it. Networking
// defaults to Tap (proxied); `--net off|tap|host` (and `-n`/`-N`) opt out.
//
// `<ref>` is resolved two ways: if it names an already-loaded box (by NAME,
// dotted display path, or numeric id) we run on that stack's top; otherwise it
// is treated as an image reference and loaded fresh (anonymous pull / archive /
// layout) first, exactly like `oci load`.
fn cli_run(args: &[String]) -> i32 {
    // A `--` splits an explicit CMD override from the run's own flags+ref.
    let sep = args.iter().position(|a| a == "--");
    let (pre, cmd) = match sep {
        Some(i) => (&args[..i], args[i + 1..].to_vec()),
        None => (args, vec![]),
    };
    let mut name: Option<String> = None;
    let mut net_mode = crate::net::NetMode::Tap; // proxied by default
    let mut reference: Option<String> = None;
    let mut it = pre.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" => match it.next() {
                Some(v) => name = Some(v.clone()),
                None => { eprintln!("sarun oci run: --name needs an argument"); return 2; }
            },
            "-n" => net_mode = crate::net::NetMode::Tap,
            "-N" => net_mode = crate::net::NetMode::Host,
            "--net" => match it.next().map(String::as_str) {
                Some(m) => match crate::net::NetMode::parse(m) {
                    Some(nm) => net_mode = nm,
                    None => {
                        eprintln!("sarun oci run: --net wants off|tap|host, got '{m}'");
                        return 2;
                    }
                },
                None => {
                    eprintln!("sarun oci run: --net needs an argument (off|tap|host)");
                    return 2;
                }
            },
            // --webcap  opt into web capture (DESIGN-web.md W2). Same env
            //           toggle the `sarun run` parser sets; read at the
            //           register message in runner::run. The browser and the
            //           crawler pass this.
            "--webcap" => unsafe { std::env::set_var("SARUN_WEBCAP", "1"); },
            // --webfilter  proxy-side adblock + rewrite (DESIGN-web.md W7).
            "--webfilter" => unsafe { std::env::set_var("SARUN_WEBFILTER", "1"); },
            "-h" | "--help" => {
                println!("usage: sarun oci run [--name NAME] [--net off|tap|host] \
                          [--webcap] [--webfilter] <ref> [-- CMD...]");
                return 0;
            }
            other if reference.is_none() => reference = Some(other.to_string()),
            other => {
                eprintln!("sarun oci run: unexpected argument '{other}'");
                return 2;
            }
        }
    }
    let Some(reference) = reference else {
        eprintln!("usage: sarun oci run [--name NAME] [--net off|tap|host] \
                   <ref> [-- CMD...]");
        return 2;
    };
    if let Err(e) = paths::ensure_dirs() {
        eprintln!("sarun oci run: {e}");
        return 1;
    }
    // Resolve <ref> to the TOP box of an image stack (loading it if needed).
    let top_id = match resolve_image_top(&reference) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("sarun oci run: {e:#}");
            return 1;
        }
    };
    // A fresh, uppercase-leading container name forces a CREATE (not a rerun)
    // and stays addressable later as a box NAME.
    let container = name.unwrap_or_else(unique_container_name);
    // Dotted session_id: the engine resolves the prefix (the top box, by id) as
    // the parent and creates a new child box named `container` under it.
    let session = format!("{top_id}.{container}");
    // Interactive when stdin is a tty (so `oci run … -- sh` gives a real shell),
    // mirroring `docker run -it`'s default for an attached terminal.
    let pty = unsafe { libc::isatty(0) == 1 };
    eprintln!("sarun oci run: container '{container}' on image top box {top_id} \
               (net={})", net_mode.as_str());
    crate::runner::run(
        Some(session),
        /* passthrough */ false,
        /* direct      */ false,
        /* env          */ false,
        /* pty          */ pty,
        /* brush        */ false,
        /* api          */ false,
        /* no_parent    */ false,
        /* readonly_parent */ false,
        /* chdir        */ None,
        net_mode,
        cmd,
    )
}

/// Resolve a run target to the box id of its image stack's TOP layer.
///
/// If `reference` names an existing box (numeric id, exact NAME, or dotted
/// display path), we walk DOWN its OCI layer children to the topmost layer and
/// run on that. Otherwise `reference` is an image ref: we load it fresh and use
/// the loader's reported top box.
/// True when this process runs inside a box (the runner sets SARUN_BROKER on
/// every box child). In-box, OCI registry work MUST go through the engine.
fn in_box() -> bool {
    std::env::var("SARUN_BROKER").is_ok_and(|s| !s.is_empty())
}

/// A control-plane connection to the engine: the per-box FD broker in-box, the
/// filesystem UDS on the host. None when no engine is reachable.
fn engine_conn() -> Option<std::os::unix::net::UnixStream> {
    if let Ok(name) = std::env::var("SARUN_BROKER") {
        if !name.is_empty() {
            return crate::runner::broker_dial(&name).ok();
        }
    }
    std::os::unix::net::UnixStream::connect(paths::sock_path()).ok()
}

/// One `{"type":"ui","verb":...}` round trip over `conn`; returns the reply's
/// `r` payload, or an error carrying the engine's message. The pull can take a
/// while — there is no read timeout, by design.
fn engine_rpc_on(mut conn: std::os::unix::net::UnixStream, verb: &str, args: Value)
    -> Result<Value> {
    use std::io::{BufRead, BufReader, Write};
    let msg = serde_json::json!({"type": "ui", "verb": verb, "args": args});
    conn.write_all(format!("{msg}\n").as_bytes()).context("engine rpc write")?;
    let mut line = String::new();
    BufReader::new(&conn).read_line(&mut line).context("engine rpc read")?;
    let rep: Value = serde_json::from_str(&line).context("engine rpc parse")?;
    if rep.get("ok").and_then(Value::as_bool) != Some(true) {
        bail!("{}", rep.get("error").and_then(Value::as_str).unwrap_or("rpc failed"));
    }
    Ok(rep.get("r").cloned().unwrap_or(Value::Null))
}

/// Pull + install an image, blocking on a fresh tokio runtime. The work behind
/// `oci load` — called directly on the no-engine host fallback, and by the
/// engine's `oci.load` RPC handler (host-side, where credentials live).
pub(crate) fn load_blocking(reference: &str, name: Option<String>) -> Result<LoadOutcome> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all().build().context("tokio init")?;
    rt.block_on(load(reference, name))
}

/// Resolve `reference` to a runnable image top-box id. The registry pull runs
/// in the ENGINE: if reachable we RPC `oci.resolve` (so credentials stay
/// host-side and an in-box caller never pulls through its own netns/FUSE);
/// otherwise (host, no `serve`) we resolve locally. An in-box caller with no
/// reachable broker is an error — falling back to a local in-box pull is
/// exactly what this change removes.
fn resolve_image_top(reference: &str) -> Result<i64> {
    if let Some(conn) = engine_conn() {
        let r = engine_rpc_on(conn, "oci.resolve", serde_json::json!([reference]))
            .with_context(|| format!("engine resolve '{reference}'"))?;
        // Surface the engine's reuse/pull note on the caller's terminal (it ran
        // host-side, so its own eprintln went to the engine log, not here).
        if let Some(note) = r.get("note").and_then(Value::as_str) {
            if !note.is_empty() { eprintln!("{note}"); }
        }
        return r.get("top_id").and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("engine oci.resolve returned no top_id"));
    }
    if in_box() {
        bail!("in a box but the engine broker is unreachable — cannot resolve \
               image '{reference}'");
    }
    let (top, note) = resolve_image_top_local(reference)?;
    if !note.is_empty() { eprintln!("{note}"); }
    Ok(top)
}

/// The actual resolution (cache lookup + pull-if-needed), run host-side: by the
/// engine's `oci.resolve` handler, or directly on the no-engine host fallback.
/// Returns the top-box id plus a human note (reuse / loaded) for the CALLER to
/// print — so the message reaches the user's terminal even when this ran in the
/// engine across an RPC.
pub(crate) fn resolve_image_top_local(reference: &str) -> Result<(i64, String)> {
    let boxes = crate::discover::discover();
    if let Some(start) = resolve_box(&boxes, reference) {
        return Ok((follow_to_top(&boxes, start), String::new()));
    }
    // Image cache — the Docker model: pull once, share the layer boxes across
    // runs. v2 keys on the MANIFEST DIGEST: probe the reference's current
    // manifest digest cheaply (a registry HEAD / a local index.json read — no
    // layer download), then:
    //   * reuse any loaded stack with that digest, so name:tag, @digest, and
    //     archive-vs-layout that point at the same image coalesce onto one stack;
    //   * a `:tag` that has MOVED (its manifest digest no longer matches the
    //     loaded stack's) falls through to a fresh pull instead of serving stale.
    let probed = probe_manifest_digest(reference).ok();
    if let Some(dg) = probed.as_deref() {
        if let Some(start) = find_loaded_by_manifest_digest(&boxes, dg) {
            let note = format!("sarun oci: reusing already-loaded image \
                                '{reference}' (manifest {dg}, box {start})");
            return Ok((follow_to_top(&boxes, start), note));
        }
    }
    // Fall back to the v1 key (exact reference string) — but only honor it when
    // the probe didn't prove the loaded stack stale. With no probe (offline /
    // unsupported source) this is exactly the v1 behavior.
    let mut prefix = String::new();
    if let Some(start) = find_loaded_by_reference(&boxes, reference) {
        let stored = boxes.get(&start).and_then(|b| b.meta.get("oci_manifest_digest"));
        let stale = matches!((probed.as_deref(), stored),
                             (Some(p), Some(s)) if p != s.as_str());
        if stale {
            prefix = format!("sarun oci: image '{reference}' tag moved (manifest \
                              changed) — re-pulling\n");
        } else {
            let note = format!("sarun oci: reusing already-loaded image \
                                '{reference}' (box {start})");
            return Ok((follow_to_top(&boxes, start), note));
        }
    }
    // Not loaded → treat as an image reference and pull it.
    let outcome = load_blocking(reference, None)
        .with_context(|| format!("load image '{reference}'"))?;
    let note = format!("{prefix}sarun oci run: loaded image '{reference}' → \
                        top box {} ({} layer(s))", outcome.top_id, outcome.n_layers);
    Ok((outcome.top_id, note))
}

/// Lowest box id of a loaded stack whose `oci_manifest_digest` matches — the
/// image-cache v2 key. The lowest id is the stack's base.
fn find_loaded_by_manifest_digest(
    boxes: &std::collections::BTreeMap<i64, crate::discover::Box_>,
    digest: &str) -> Option<i64> {
    if digest.is_empty() { return None; }
    boxes.values()
        .filter(|b| b.meta.get("oci_manifest_digest").map(String::as_str) == Some(digest))
        .map(|b| b.box_id)
        .min()
}

/// Cheaply determine `reference`'s current manifest digest WITHOUT downloading
/// layers: a registry manifest HEAD, or the `index.json` of a local
/// archive/layout. Used to key the image cache (reuse / moved-tag detection).
fn probe_manifest_digest(reference: &str) -> Result<String> {
    if let Some(p) = reference.strip_prefix("oci-archive:") {
        return archive_index_manifest_digest(Path::new(p));
    }
    if let Some(p) = reference.strip_prefix("oci-layout:") {
        let idx = std::fs::read(Path::new(p).join("index.json"))
            .with_context(|| format!("read {}/index.json", p))?;
        return index_manifest_digest(&idx);
    }
    // Registry: a cheap manifest fetch (no blobs).
    let r = Reference::from_str(reference)
        .with_context(|| format!("parse reference '{reference}'"))?;
    let auth = registry_auth_for(reference);
    let client = Client::new(ClientConfig::default());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all().build().context("tokio init")?;
    rt.block_on(client.fetch_manifest_digest(&r, &auth))
        .context("fetch manifest digest")
}

/// Read just `index.json` out of an oci-archive tar (top-level entry) and return
/// its platform-matched manifest digest — no layer extraction.
fn archive_index_manifest_digest(path: &Path) -> Result<String> {
    let file = File::open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let mut ar = tar::Archive::new(file);
    for entry in ar.entries().context("read archive entries")? {
        let mut e = entry.context("archive entry")?;
        let ep = e.path().context("entry path")?.into_owned();
        if ep.file_name().and_then(|s| s.to_str()) == Some("index.json")
            && ep.components().count() == 1
        {
            let mut buf = Vec::new();
            e.read_to_end(&mut buf).context("read index.json")?;
            return index_manifest_digest(&buf);
        }
    }
    bail!("oci-archive {} has no top-level index.json", path.display())
}

/// Lowest box id of an already-loaded stack whose `oci_reference` meta matches
/// `reference` — the v1 image-cache key (exact reference string). v2
/// (find_loaded_by_manifest_digest) keys on the manifest digest and is tried
/// first; this remains the fallback when no digest probe is available. The
/// lowest id is the stack's base, so follow_to_top reaches that stack's top.
fn find_loaded_by_reference(boxes: &std::collections::BTreeMap<i64, crate::discover::Box_>,
                            reference: &str) -> Option<i64> {
    boxes.values()
        .filter(|b| b.meta.get("oci_reference").map(String::as_str) == Some(reference))
        .map(|b| b.box_id)
        .min()
}

/// Box id for `ident` as a numeric id, an exact NAME, or a dotted display path
/// (the same identifiers `control::resolve` accepts, replicated here so the CLI
/// path doesn't depend on the control module's private resolver).
fn resolve_box(boxes: &std::collections::BTreeMap<i64, crate::discover::Box_>,
               ident: &str) -> Option<i64> {
    if let Ok(id) = ident.parse::<i64>() {
        if boxes.contains_key(&id) { return Some(id); }
    }
    boxes.values()
        .find(|b| b.name == ident
              || crate::discover::display_path(boxes, b.box_id) == ident)
        .map(|b| b.box_id)
}

/// Follow the OCI layer chain DOWN from `start` to the topmost layer box. The
/// loader builds a linear stack (each layer's parent is the one below), so we
/// step to the unique child that is itself an OCI layer — skipping any
/// non-layer children (e.g. previous run-containers parented under the image).
fn follow_to_top(boxes: &std::collections::BTreeMap<i64, crate::discover::Box_>,
                 start: i64) -> i64 {
    let mut cur = start;
    let mut seen = std::collections::HashSet::new();
    loop {
        if !seen.insert(cur) { break; } // cycle guard
        // Step to the unique child that is itself an OCI image layer
        // (loader-stamped `oci_layer_index` meta), skipping run-containers.
        let next = boxes.values()
            .find(|b| b.parent == Some(cur)
                  && b.meta.contains_key("oci_layer_index"))
            .map(|b| b.box_id);
        match next {
            Some(n) => cur = n,
            None => break,
        }
    }
    cur
}

/// A unique, valid (`valid_name`: uppercase-leading [A-Z0-9-]) container box
/// name, so each run CREATEs a fresh box instead of re-running a sibling.
fn unique_container_name() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos()).unwrap_or(0) as u64;
    format!("R{:X}", n & 0xFFFF_FFFF)
}

// ── `sarun oci build` ────────────────────────────────────────────────────────
// Execute a Dockerfile/Containerfile into a chain of at-rest sarun boxes — one
// box per filesystem-mutating instruction, mirroring how `oci load` lays an
// image out. The accumulated image config (env / workdir / user / cmd /
// entrypoint / labels / …) is stamped as `oci_config` meta on each new layer so
// `oci run` (and the engine's chain-walk) picks it up, exactly like a loaded
// image.
//
// FROM resolves its base the same way `oci run` does (registry / archive /
// layout / an already-loaded box, or `scratch` for an empty rootfs) and starts
// a fresh OWNED layer on top — the base image's own boxes are never mutated.
// RUN runs a real box on the current layer via the engine (so it needs a
// running sarun engine/UI) and adopts the box it leaves behind (Rust boxes are
// at-rest the instant they exit) as the next layer. COPY/ADD ingest the build
// context straight into a new layer box.
//
// Scope (v1): single- and multi-stage (`FROM … AS name`, `FROM name`,
// `COPY --from=<stage|image>`); RUN (shell + exec form, honoring SHELL);
// COPY/ADD of local context files+dirs with numeric --chown and octal --chmod,
// glob sources, ADD-of-URL fetch and ADD-of-local-tar (gzip/zstd/xz/bzip2/
// plain) auto-extract; ENV/ARG/WORKDIR/USER/CMD/ENTRYPOINT/LABEL/EXPOSE/VOLUME/
// SHELL/STOPSIGNAL/ONBUILD/HEALTHCHECK carried into the image config; `-t` tag,
// `-f` file, `--build-arg`, `--net`. A `FROM` registry pull authenticates via
// the host Docker config (see registry_auth_for).
fn cli_build(args: &[String]) -> i32 {
    let mut tag: Option<String> = None;
    let mut file: Option<String> = None;
    let mut net_mode = crate::net::NetMode::Tap;
    let mut build_args: Vec<(String, String)> = Vec::new();
    let mut context: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-t" | "--tag" => match it.next() {
                Some(v) => tag = Some(v.clone()),
                None => { eprintln!("sarun oci build: {a} needs an argument"); return 2; }
            },
            "-f" | "--file" => match it.next() {
                Some(v) => file = Some(v.clone()),
                None => { eprintln!("sarun oci build: {a} needs an argument"); return 2; }
            },
            "-n" => net_mode = crate::net::NetMode::Tap,
            "-N" => net_mode = crate::net::NetMode::Host,
            "--net" => match it.next().map(String::as_str) {
                Some(m) => match crate::net::NetMode::parse(m) {
                    Some(nm) => net_mode = nm,
                    None => {
                        eprintln!("sarun oci build: --net wants off|tap|host, got '{m}'");
                        return 2;
                    }
                },
                None => { eprintln!("sarun oci build: --net needs an argument"); return 2; }
            },
            "--build-arg" => match it.next() {
                Some(kv) => match kv.split_once('=') {
                    Some((k, v)) => build_args.push((k.to_string(), v.to_string())),
                    None => { eprintln!("sarun oci build: --build-arg wants K=V"); return 2; }
                },
                None => { eprintln!("sarun oci build: --build-arg needs an argument"); return 2; }
            },
            "-h" | "--help" => {
                println!("usage: sarun oci build [-t NAME] [-f FILE] [--net MODE] \
                          [--build-arg K=V]... [CONTEXT]");
                return 0;
            }
            other if context.is_none() => context = Some(other.to_string()),
            other => { eprintln!("sarun oci build: unexpected argument '{other}'"); return 2; }
        }
    }
    let context = PathBuf::from(context.unwrap_or_else(|| ".".to_string()));
    let df_path = file.map(PathBuf::from)
        .unwrap_or_else(|| context.join("Dockerfile"));
    let text = match std::fs::read_to_string(&df_path) {
        Ok(t) => t,
        Err(e) => { eprintln!("sarun oci build: read {}: {e}", df_path.display()); return 1; }
    };
    // In a box the build MUST run host-side in the engine: its layer boxes
    // (FROM/COPY/ADD/RUN) have to be created in the engine's state, not through
    // the box's own FUSE (which would make them ephemeral box-overlay files).
    // Ship the context + Dockerfile to the engine, which runs the build in a
    // host-side worker and returns its output. On the host we build locally
    // (live output; unchanged).
    if in_box() {
        return build_via_engine(&context, &text, tag, net_mode, &build_args);
    }
    // RUN steps need a live engine to execute in; fail fast with a clear message
    // rather than mid-build.
    if let Ok(df) = crate::dockerfile::Dockerfile::parse(&text) {
        let has_run = df.instructions.iter()
            .any(|(_, i)| matches!(i, crate::dockerfile::Instruction::Run(_)));
        if has_run && std::os::unix::net::UnixStream::connect(paths::sock_path()).is_err() {
            eprintln!("sarun oci build: this Dockerfile has RUN steps, which execute \
                       in a box — start the sarun engine/UI first \
                       (control socket {}).", paths::sock_path().display());
            return 3;
        }
    }
    match run_dockerfile(context, &text, tag, net_mode, build_args) {
        Ok(_) => 0,
        Err(e) => { eprintln!("sarun oci build: {e:#}"); 1 }
    }
}

/// Run a parsed Dockerfile to completion against `context`, returning the top
/// layer box id. Shared by the host build and the engine-side build worker.
fn run_dockerfile(context: PathBuf, text: &str, tag: Option<String>,
                  net_mode: crate::net::NetMode,
                  build_args: Vec<(String, String)>) -> Result<i64> {
    let df = crate::dockerfile::Dockerfile::parse(text).map_err(|e| anyhow!("{e}"))?;
    paths::ensure_dirs().map_err(|e| anyhow!("ensure dirs: {e}"))?;
    let mut b = Builder::new(context, net_mode, build_args);
    for (line, instr) in &df.instructions {
        b.exec(instr).with_context(|| format!("Dockerfile line {line}"))?;
    }
    b.finish(tag)?;
    b.current.ok_or_else(|| anyhow!("build produced no top box"))
}

/// In-box `oci build`: pack the context (gzip tar, base64) and ship it with the
/// Dockerfile to the engine's `oci.build` RPC, which runs the build host-side.
/// The engine returns the worker's combined output + exit code; we replay both.
fn build_via_engine(context: &Path, df_text: &str, tag: Option<String>,
                    net_mode: crate::net::NetMode,
                    build_args: &[(String, String)]) -> i32 {
    let tar_b64 = match tar_gz_dir_b64(context) {
        Ok(s) => s,
        Err(e) => { eprintln!("sarun oci build: pack context: {e:#}"); return 1; }
    };
    let bargs: Vec<Value> = build_args.iter()
        .map(|(k, v)| serde_json::json!([k, v])).collect();
    let spec = serde_json::json!([{
        "context_tar_gz": tar_b64,
        "dockerfile": df_text,
        "tag": tag,
        "net": net_mode.as_str(),
        "build_args": bargs,
    }]);
    let Some(conn) = engine_conn() else {
        eprintln!("sarun oci build: in a box but the engine broker is unreachable");
        return 1;
    };
    match engine_rpc_on(conn, "oci.build", spec) {
        Ok(r) => {
            if let Some(log) = r.get("log").and_then(Value::as_str) { print!("{log}"); }
            r.get("code").and_then(Value::as_i64).unwrap_or(1) as i32
        }
        Err(e) => { eprintln!("sarun oci build: {e}"); 1 }
    }
}

/// Pack a directory as a gzip'd tar, base64-encoded — the in-box build context
/// shipped to the engine.
fn tar_gz_dir_b64(dir: &Path) -> Result<String> {
    use base64::{Engine as _, prelude::BASE64_STANDARD};
    let mut gz = flate2::write::GzEncoder::new(Vec::new(),
                                               flate2::Compression::default());
    {
        let mut tb = tar::Builder::new(&mut gz);
        tb.append_dir_all(".", dir).context("tar context")?;
        tb.finish().context("finish tar")?;
    }
    Ok(BASE64_STANDARD.encode(gz.finish().context("gzip context")?))
}

/// The engine-side `oci.build` handler: unpack the shipped context, run the
/// build in a HOST worker process (`/proc/self/exe oci __build-worker`) so its
/// layer boxes are created host-side, and return the worker's combined output,
/// exit code, and top box id. Runs on the connection's own handler thread, so a
/// long build doesn't block the engine's main loop.
pub(crate) fn build_in_engine(spec: &Value) -> Result<Value> {
    use std::process::{Command, Stdio};
    let tar_b64 = spec.get("context_tar_gz").and_then(Value::as_str)
        .ok_or_else(|| anyhow!("oci.build: missing context_tar_gz"))?;
    let df_text = spec.get("dockerfile").and_then(Value::as_str).unwrap_or("");
    let tag = spec.get("tag").and_then(Value::as_str);
    let net = spec.get("net").and_then(Value::as_str).unwrap_or("tap");
    let build_args: Vec<(String, String)> = spec.get("build_args")
        .and_then(Value::as_array).map(|a| a.iter().filter_map(|kv| {
            let kv = kv.as_array()?;
            Some((kv.first()?.as_str()?.to_string(), kv.get(1)?.as_str()?.to_string()))
        }).collect()).unwrap_or_default();

    let stamp = format!("{}-{}", std::process::id(), now_ns());
    let ctx = paths::runtime_home().join(format!("oci-build-{stamp}"));
    std::fs::create_dir_all(&ctx).context("create build context dir")?;
    let result_file = paths::runtime_home().join(format!("oci-build-{stamp}.json"));
    let _guard = TmpCleanup { dir: ctx.clone(), file: result_file.clone() };
    unpack_tar_gz_b64(tar_b64, &ctx)?;
    let df_path = ctx.join(".sarun-Dockerfile");
    std::fs::write(&df_path, df_text).context("write shipped Dockerfile")?;

    let mut cmd = Command::new("/proc/self/exe");
    cmd.arg("oci").arg("__build-worker")
        .arg("--context").arg(&ctx)
        .arg("-f").arg(&df_path)
        .arg("--net").arg(net)
        .arg("--result-file").arg(&result_file);
    if let Some(t) = tag { cmd.arg("-t").arg(t); }
    for (k, v) in &build_args { cmd.arg("--build-arg").arg(format!("{k}={v}")); }
    let out = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .output().context("spawn build worker")?;
    let mut log = String::from_utf8_lossy(&out.stdout).into_owned();
    log.push_str(&String::from_utf8_lossy(&out.stderr));
    let code = out.status.code().unwrap_or(1);
    let top_id = std::fs::read_to_string(&result_file).ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("top_id").and_then(Value::as_i64));
    Ok(serde_json::json!({"code": code, "log": log, "top_id": top_id}))
}

/// Best-effort cleanup of the engine-side build temp dir + result file.
struct TmpCleanup { dir: PathBuf, file: PathBuf }
impl Drop for TmpCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_file(&self.file);
    }
}

fn unpack_tar_gz_b64(b64: &str, dest: &Path) -> Result<()> {
    use base64::{Engine as _, prelude::BASE64_STANDARD};
    let bytes = BASE64_STANDARD.decode(b64.trim()).context("decode context")?;
    let gz = flate2::read::GzDecoder::new(&bytes[..]);
    tar::Archive::new(gz).unpack(dest).context("unpack context")?;
    Ok(())
}

/// The hidden build worker: a HOST process the engine spawns so the build's
/// layer boxes land in the engine's state. Runs the Dockerfile against the
/// already-unpacked context and writes `{"top_id":N}` to --result-file.
fn cli_build_worker(args: &[String]) -> i32 {
    let mut context: Option<String> = None;
    let mut file: Option<String> = None;
    let mut tag: Option<String> = None;
    let mut net_mode = crate::net::NetMode::Tap;
    let mut build_args: Vec<(String, String)> = Vec::new();
    let mut result_file: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--context" => context = it.next().cloned(),
            "-f" | "--file" => file = it.next().cloned(),
            "-t" | "--tag" => tag = it.next().cloned(),
            "--result-file" => result_file = it.next().cloned(),
            "--net" => match it.next().map(String::as_str).and_then(crate::net::NetMode::parse) {
                Some(nm) => net_mode = nm,
                None => { eprintln!("__build-worker: bad --net"); return 2; }
            },
            "--build-arg" => if let Some((k, v)) = it.next().and_then(|kv| kv.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))) { build_args.push((k, v)); },
            other => { eprintln!("__build-worker: unexpected arg '{other}'"); return 2; }
        }
    }
    let context = PathBuf::from(context.unwrap_or_else(|| ".".to_string()));
    let df_path = file.map(PathBuf::from).unwrap_or_else(|| context.join("Dockerfile"));
    let text = match std::fs::read_to_string(&df_path) {
        Ok(t) => t,
        Err(e) => { eprintln!("sarun oci build: read {}: {e}", df_path.display()); return 1; }
    };
    match run_dockerfile(context, &text, tag, net_mode, build_args) {
        Ok(top) => {
            if let Some(rf) = result_file {
                let _ = std::fs::write(&rf, serde_json::json!({"top_id": top}).to_string());
            }
            0
        }
        Err(e) => { eprintln!("sarun oci build: {e:#}"); 1 }
    }
}

use crate::dockerfile::{Cmdline, Instruction};
use crate::dockerfile::expand;

/// Render a Cmdline as it reads in a Dockerfile, for history/frame text: the
/// shell form is the raw command string; the exec form is its JSON array.
fn cmdline_display(c: &Cmdline) -> String {
    match c {
        Cmdline::Shell(s) => s.clone(),
        Cmdline::Exec(args) => serde_json::to_string(args).unwrap_or_default(),
    }
}

/// Accumulating build state: the current top layer box, the image config being
/// assembled, build-time variables (ARG + ENV) for `$VAR` expansion, and the
/// named-stage table for multi-stage `FROM name` / `FROM … AS name`.
/// Clone-able so the interactive author can snapshot state before each
/// instruction and roll back on `undo`.
#[derive(Clone)]
struct Builder {
    context: PathBuf,
    net_mode: crate::net::NetMode,
    vars: HashMap<String, String>,
    // image config in progress
    env: Vec<(String, String)>,
    workdir: String,
    user: Option<String>,
    cmd: Option<Vec<String>>,
    entrypoint: Option<Vec<String>>,
    labels: Vec<(String, String)>,
    exposed: Vec<String>,
    volumes: Vec<String>,
    shell: Vec<String>,
    shell_set: bool,
    stopsignal: Option<String>,
    onbuild: Vec<String>,
    healthcheck: Option<crate::dockerfile::HealthcheckSpec>,
    // A base image's Healthcheck JSON (already nanosecond-form), carried
    // verbatim unless this build overrides it with its own HEALTHCHECK.
    healthcheck_raw: Option<serde_json::Value>,
    // build position
    current: Option<i64>,
    started: bool,
    step: usize,
    stages: HashMap<String, i64>,
    pending_stage: Option<String>,
    // OCI config `history` accumulated across instructions (base image's history
    // seeded first, then one entry per instruction). Layer-creating instructions
    // are empty_layer=false; config-only ones true. Projected into the image
    // config so `docker history` (and the Dockerfile emitter) see the recipe.
    history: Vec<Value>,
    // Per-box directive list (op+text), accumulated against the box that was
    // current when each instruction ran, stamped as each box's `frame` meta at
    // finish(). One box = the directives that built it — what the Dockerfile
    // emitter walks and the interactive authoring/undo path reads.
    frames: std::collections::HashMap<i64, Vec<Value>>,
    // Per-box COPY hint: the longest common path of the box's own writes (one
    // pass over its layer, cheap). Emitted as a commented `#COPY` line so an
    // author can uncomment + set the source if those files came from the host.
    // Set only for authored RUN steps; empty / "/" (scattered writes) is skipped.
    copy_hints: std::collections::HashMap<i64, String>,
}

impl Builder {
    fn new(context: PathBuf, net_mode: crate::net::NetMode,
           build_args: Vec<(String, String)>) -> Self {
        let mut vars = HashMap::new();
        for (k, v) in build_args { vars.insert(k, v); }
        Builder {
            context, net_mode, vars,
            env: Vec::new(), workdir: "/".to_string(), user: None,
            cmd: None, entrypoint: None, labels: Vec::new(),
            exposed: Vec::new(), volumes: Vec::new(),
            shell: vec!["/bin/sh".to_string(), "-c".to_string()],
            shell_set: false,
            stopsignal: None, onbuild: Vec::new(), healthcheck: None,
            healthcheck_raw: None,
            current: None, started: false, step: 0,
            stages: HashMap::new(), pending_stage: None,
            history: Vec::new(),
            frames: std::collections::HashMap::new(),
            copy_hints: std::collections::HashMap::new(),
        }
    }

    /// Run one instruction, then record it in the build history (and stamp a
    /// per-box `frame` for layer-creating instructions).
    fn exec(&mut self, instr: &Instruction) -> Result<()> {
        self.dispatch(instr)?;
        self.record_history(instr);
        Ok(())
    }

    /// Append the instruction to `self.history` (OCI config history) and, for a
    /// layer-creating instruction, stamp the new box's `frame` meta with the
    /// structured directive — the per-box record the Dockerfile emitter and the
    /// interactive authoring/undo path read. FROM/ARG/unsupported add nothing.
    fn record_history(&mut self, instr: &Instruction) {
        if !self.started { return; }
        // (op, text, empty_layer). `text` is the directive body as it reads in a
        // Dockerfile (after the keyword) — so `<op> <text>` reconstructs the line.
        let (op, text, empty_layer): (&str, String, bool) = match instr {
            Instruction::Run(c) => ("RUN", cmdline_display(c), false),
            Instruction::Copy { sources, dest, from, .. } => {
                let pfx = from.as_ref().map(|s| format!("--from={s} ")).unwrap_or_default();
                ("COPY", format!("{pfx}{} {dest}", sources.join(" ")), false)
            }
            Instruction::Add { sources, dest, .. } =>
                ("ADD", format!("{} {dest}", sources.join(" ")), false),
            Instruction::Env(p) => ("ENV",
                p.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" "), true),
            Instruction::Workdir(p) => ("WORKDIR", p.clone(), true),
            Instruction::User(u) => ("USER", u.clone(), true),
            Instruction::Cmd(c) => ("CMD", cmdline_display(c), true),
            Instruction::Entrypoint(c) => ("ENTRYPOINT", cmdline_display(c), true),
            Instruction::Label(p) => ("LABEL",
                p.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" "), true),
            Instruction::Expose(s) => ("EXPOSE", s.clone(), true),
            Instruction::Volume(v) => ("VOLUME", v.join(" "), true),
            Instruction::Shell(v) => ("SHELL",
                serde_json::to_string(v).unwrap_or_default(), true),
            Instruction::Stopsignal(s) => ("STOPSIGNAL", s.clone(), true),
            Instruction::Onbuild(s) => ("ONBUILD", s.clone(), true),
            Instruction::Healthcheck(_) => ("HEALTHCHECK", String::new(), true),
            Instruction::From { .. } | Instruction::Arg { .. }
            | Instruction::Unsupported { .. } => return,
        };
        let created_by = if text.is_empty() { op.to_string() }
                         else { format!("{op} {text}") };
        // `created` (RFC3339) is optional in the OCI spec; omit it rather than
        // pull in a date dep.
        self.history.push(serde_json::json!({
            "created_by": created_by, "empty_layer": empty_layer }));
        // Attach the directive to the box that was current when it ran. Frames
        // are stamped in finish().
        if let Some(id) = self.current {
            self.frames.entry(id).or_default()
                .push(serde_json::json!({"op": op, "text": text}));
        }
        // Refresh the current box's stamped config so its oci_config carries the
        // history so far; finish() re-stamps the top with the complete list.
        self.stamp_current();
    }

    fn dispatch(&mut self, instr: &Instruction) -> Result<()> {
        match instr {
            Instruction::From { image, platform: _, stage_as } => self.do_from(image, stage_as),
            Instruction::Arg { name, default } => {
                // ARG may precede FROM (global). It only seeds a default when the
                // var isn't already set (a --build-arg or earlier ARG wins).
                if !self.vars.contains_key(name) {
                    let d = default.as_deref().map(|d| expand(d, &self.vars))
                        .unwrap_or_default();
                    self.vars.insert(name.clone(), d);
                }
                Ok(())
            }
            _ if !self.started => bail!("instruction before any FROM"),
            Instruction::Run(c) => self.do_run(c),
            Instruction::Copy { sources, dest, from, chown, chmod } => {
                self.do_copy(sources, dest, from.as_deref(),
                             chown.as_deref(), chmod.as_deref(), false)
            }
            Instruction::Add { sources, dest, chown, chmod } => {
                self.do_copy(sources, dest, None,
                             chown.as_deref(), chmod.as_deref(), true)
            }
            Instruction::Env(pairs) => {
                for (k, v) in pairs {
                    let ve = expand(v, &self.vars);
                    self.set_env(k, &ve);
                }
                self.stamp_current();
                Ok(())
            }
            Instruction::Workdir(p) => self.do_workdir(p),
            Instruction::User(u) => {
                self.user = Some(expand(u, &self.vars));
                self.stamp_current();
                Ok(())
            }
            Instruction::Cmd(c) => { self.cmd = Some(self.cmdline_vec(c)); self.stamp_current(); Ok(()) }
            Instruction::Entrypoint(c) => {
                self.entrypoint = Some(self.cmdline_vec(c)); self.stamp_current(); Ok(())
            }
            Instruction::Label(pairs) => {
                for (k, v) in pairs {
                    let ve = expand(v, &self.vars);
                    self.labels.retain(|(ek, _)| ek != k);
                    self.labels.push((k.clone(), ve));
                }
                self.stamp_current();
                Ok(())
            }
            Instruction::Expose(s) => { self.exposed.push(expand(s, &self.vars)); self.stamp_current(); Ok(()) }
            Instruction::Volume(v) => {
                for p in v { self.volumes.push(expand(p, &self.vars)); }
                self.stamp_current();
                Ok(())
            }
            Instruction::Shell(v) => {
                self.shell = v.iter().map(|a| expand(a, &self.vars)).collect();
                self.shell_set = true;
                self.stamp_current();
                Ok(())
            }
            Instruction::Stopsignal(s) => {
                self.stopsignal = Some(expand(s, &self.vars));
                self.stamp_current();
                Ok(())
            }
            Instruction::Onbuild(s) => {
                self.onbuild.push(expand(s, &self.vars));
                self.stamp_current();
                Ok(())
            }
            Instruction::Healthcheck(spec) => {
                self.healthcheck = Some(spec.clone());
                self.healthcheck_raw = None; // this build's HEALTHCHECK wins
                self.stamp_current();
                Ok(())
            }
            Instruction::Unsupported { verb, .. } => {
                eprintln!("sarun oci build: warning: {verb} is recognized but not \
                           acted on (skipped)");
                Ok(())
            }
        }
    }

    fn do_from(&mut self, image: &str, stage_as: &Option<String>) -> Result<()> {
        // Close out the previous stage under its AS name, if any.
        if let (Some(name), Some(cur)) = (self.pending_stage.take(), self.current) {
            self.stages.insert(name, cur);
        }
        let img = expand(image, &self.vars);
        if img == "scratch" {
            self.begin_stage(None, None)?;
        } else if let Some(&sid) = self.stages.get(&img) {
            self.begin_stage(Some(sid), Some(sid))?;
        } else {
            let top = resolve_image_top(&img)
                .with_context(|| format!("FROM {img}"))?;
            self.begin_stage(Some(top), Some(top))?;
        }
        self.pending_stage = stage_as.clone();
        self.started = true;
        eprintln!("sarun oci build: FROM {img} → layer box {}", self.current.unwrap());
        Ok(())
    }

    /// Start a fresh OWNED layer on top of `parent` (None = scratch / no host
    /// fall-through), seeding the config from `seed_from`'s `oci_config` so a
    /// base image's env/cmd/etc. carry into the build.
    fn begin_stage(&mut self, parent: Option<i64>, seed_from: Option<i64>) -> Result<()> {
        self.reset_config();
        if let Some(s) = seed_from { self.seed_config_from(s); }
        let id = mint_box_id();
        let bx = BoxState::create(id).with_context(|| format!("create box {id}"))?;
        bx.set_meta("name", &format!("B{id}"));
        match parent {
            Some(p) => { bx.set_parent(Some(p)); bx.set_meta("parent_box_id", &p.to_string()); }
            None => { bx.set_no_host_fallback(true); bx.set_meta("no_host_fallback", "1"); }
        }
        self.current = Some(id);
        // Register this build box (even with no directives yet) so it's
        // recognized as a build step (vs the base image) and gets a `frame`.
        self.frames.entry(id).or_default();
        self.stamp(id);
        Ok(())
    }

    fn do_run(&mut self, c: &Cmdline) -> Result<()> {
        let parent = self.current.ok_or_else(|| anyhow!("RUN with no current layer"))?;
        // Make sure the current layer carries the latest config so the engine
        // applies env/workdir/user to the RUN box via the parent-chain walk.
        self.stamp(parent);
        self.step += 1;
        let step_name = format!("S{}", self.step);
        let session = format!("{parent}.{step_name}");
        let cmd = self.cmdline_vec(c);
        eprintln!("sarun oci build: RUN ({}) {:?}", step_name, cmd);
        let code = crate::runner::run(
            Some(session),
            /* passthrough */ false, /* direct */ false, /* env */ false,
            /* pty */ false, /* brush */ false, /* api */ false,
            /* no_parent */ false, /* readonly_parent */ false, /* chdir */ None,
            self.net_mode, cmd,
        );
        if code != 0 {
            bail!("RUN step '{step_name}' exited with status {code}");
        }
        // The box the engine left behind (at-rest the instant it exited) is the
        // new layer. Look it up by name under its parent.
        let id = find_child_named(parent, &step_name)
            .ok_or_else(|| anyhow!("RUN step '{step_name}' produced no box"))?;
        self.current = Some(id);
        self.stamp(id); // carry config forward onto the new layer
        Ok(())
    }

    fn do_workdir(&mut self, p: &str) -> Result<()> {
        let p = expand(p, &self.vars);
        self.workdir = if p.starts_with('/') {
            p
        } else {
            format!("{}/{}", self.workdir.trim_end_matches('/'), p)
        };
        // Materialize the directory so a subsequent RUN's cwd exists (Docker
        // creates WORKDIR if missing).
        let rel = normalize_rel(&self.workdir);
        if !rel.is_empty() {
            let cur = self.current.ok_or_else(|| anyhow!("WORKDIR with no layer"))?;
            let bx = BoxState::create(cur)?;
            ensure_dir_chain(&bx, &rel);
        }
        self.stamp_current();
        Ok(())
    }

    fn do_copy(&mut self, sources: &[String], dest: &str, from: Option<&str>,
               chown: Option<&str>, chmod: Option<&str>, is_add: bool) -> Result<()> {
        let verb = if is_add { "ADD" } else { "COPY" };
        let srcs: Vec<String> = sources.iter().map(|s| expand(s, &self.vars)).collect();
        let dst = expand(dest, &self.vars);
        let owner = chown.and_then(parse_chown);
        let mode = chmod.and_then(|m| u32::from_str_radix(m.trim(), 8).ok());
        // A new layer box holds this instruction's files.
        let id = mint_box_id();
        let bx = BoxState::create(id).with_context(|| format!("create box {id}"))?;
        bx.set_meta("name", &format!("B{id}"));
        if let Some(p) = self.current {
            bx.set_parent(Some(p)); bx.set_meta("parent_box_id", &p.to_string());
        }
        // dest is a directory if it ends in `/` or there are multiple sources.
        let dst_is_dir = dst.ends_with('/') || srcs.len() > 1;
        let dst_rel = self.box_rel(&dst);
        if let Some(from) = from.map(|f| expand(f, &self.vars)) {
            self.copy_from_stage(&bx, &from, &srcs, &dst_rel, dst_is_dir, owner, mode)?;
        } else {
            self.copy_from_context(&bx, &srcs, &dst_rel, dst_is_dir, owner, mode, is_add)?;
        }
        self.current = Some(id);
        self.stamp(id);
        eprintln!("sarun oci build: {verb} {srcs:?} → {dst} (layer box {id})");
        Ok(())
    }

    /// COPY/ADD whose sources live in the build context (no `--from`). Handles
    /// glob expansion, ADD-of-URL fetch, and ADD-of-local-tar auto-extract.
    fn copy_from_context(&self, bx: &BoxState, srcs: &[String], dst_rel: &str,
                         dst_is_dir: bool, owner: Option<(u32, u32)>,
                         mode: Option<u32>, is_add: bool) -> Result<()> {
        let verb = if is_add { "ADD" } else { "COPY" };
        let canon_ctx = self.context.canonicalize().unwrap_or_else(|_| self.context.clone());
        for src in srcs {
            // ADD <url>: fetch into dest. Docker does NOT auto-extract URL
            // sources (only local archives), so this is a plain file write.
            if is_add && (src.starts_with("http://") || src.starts_with("https://")) {
                let bytes = fetch_url(src)
                    .with_context(|| format!("ADD fetch '{src}'"))?;
                let base = url_basename(src);
                if base.is_empty() {
                    bail!("ADD URL '{src}' has no filename to write to");
                }
                let target = join_dest(dst_rel, dst_is_dir, &base);
                put_file_bytes(bx, &target, &bytes, mode.unwrap_or(0o644), now_ns())?;
                if let Some((u, g)) = owner { bx.set_owner(&target, u, g); }
                continue;
            }
            // Local source(s): glob-expand if the pattern has wildcards.
            let matches: Vec<PathBuf> = if src.contains(['*', '?', '[']) {
                let m = self.glob_context(src)?;
                if m.is_empty() {
                    bail!("{verb} pattern '{src}' matched no files in the build context");
                }
                m
            } else {
                vec![self.context.join(src)]
            };
            for src_abs in matches {
                let canon_src = src_abs.canonicalize()
                    .with_context(|| format!("{verb} source '{}' not found in context",
                                             src_abs.display()))?;
                if !canon_src.starts_with(&canon_ctx) {
                    bail!("{verb} source '{}' escapes the build context", src_abs.display());
                }
                // ADD auto-extracts a local tar (gzip/zstd/xz/bzip2/plain).
                if is_add && canon_src.is_file() {
                    match archive_kind(&canon_src)? {
                        ArchiveKind::Tar => {
                            let blob = std::fs::read(&canon_src)?;
                            extract_tar_into(bx, &blob, dst_rel, owner, mode)
                                .with_context(|| format!("ADD extract '{}'",
                                                         canon_src.display()))?;
                            continue;
                        }
                        ArchiveKind::NotArchive => {} // plain copy below
                    }
                }
                if canon_src.is_dir() {
                    copy_tree(bx, &canon_src, dst_rel, owner, mode)?;
                } else {
                    let base = canon_src.file_name()
                        .map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let target = join_dest(dst_rel, dst_is_dir, &base);
                    copy_file(bx, &canon_src, &target, owner, mode)?;
                }
            }
        }
        Ok(())
    }

    /// `COPY --from=<stage|image> SRC… DST` — read SRC from another stage's (or
    /// an external image's) MERGED view and stage it into this layer. We reuse
    /// the Overlay resolver host-side (no FUSE mount) so the parent-chain /
    /// whiteout / opaque semantics are exactly what a box would see.
    fn copy_from_stage(&self, bx: &BoxState, from: &str, srcs: &[String],
                       dst_rel: &str, dst_is_dir: bool, owner: Option<(u32, u32)>,
                       mode: Option<u32>) -> Result<()> {
        let src_id = if let Some(&sid) = self.stages.get(from) {
            sid
        } else {
            resolve_image_top(from)
                .with_context(|| format!("COPY --from={from}"))?
        };
        let ov = crate::overlay::Overlay::new(PathBuf::from("/"));
        for src in srcs {
            let rel = normalize_rel(src);
            match ov.box_path_kind(src_id, &rel) {
                'd' => {
                    let base_dst = if dst_rel.is_empty() { String::new() }
                                   else { dst_rel.to_string() };
                    copy_box_tree(&ov, src_id, &rel, bx, &base_dst, owner, mode)?;
                }
                'f' => {
                    let bytes = ov.box_read_file(src_id, &rel)
                        .with_context(|| format!("COPY --from={from} read '{src}'"))?;
                    let src_mode = mode
                        .or_else(|| ov.box_file_mode(src_id, &rel))
                        .unwrap_or(0o644);
                    let base = leaf_name(&rel);
                    let target = join_dest(dst_rel, dst_is_dir, &base);
                    put_file_bytes(bx, &target, &bytes, src_mode, now_ns())?;
                    if let Some((u, g)) = owner { bx.set_owner(&target, u, g); }
                }
                'l' => {
                    let tgt = ov.box_read_file(src_id, &rel)
                        .with_context(|| format!("COPY --from={from} readlink '{src}'"))?;
                    let base = leaf_name(&rel);
                    let target = join_dest(dst_rel, dst_is_dir, &base);
                    bx.set_symlink(&target,
                        Path::new(&String::from_utf8_lossy(&tgt).into_owned()), 0);
                    if let Some((u, g)) = owner { bx.set_owner(&target, u, g); }
                }
                _ => bail!("COPY --from={from}: source '{src}' not found"),
            }
        }
        Ok(())
    }

    /// Glob a context-relative pattern, returning matched absolute paths.
    fn glob_context(&self, pattern: &str) -> Result<Vec<PathBuf>> {
        let joined = self.context.join(pattern);
        let pat = joined.to_string_lossy();
        let mut out = Vec::new();
        for entry in glob::glob(&pat)
            .map_err(|e| anyhow!("bad glob pattern '{pattern}': {e}"))?
        {
            if let Ok(p) = entry { out.push(p); }
        }
        Ok(out)
    }

    /// Resolve a Dockerfile dest path to a box-relative path (relative to the
    /// current WORKDIR, leading slash stripped — box rels never start with `/`),
    /// with `.`/empty components collapsed and `..` popped. Without this a
    /// `COPY x ./x` under WORKDIR /opt would store `opt/./x`, which FUSE could
    /// never resolve (the kernel strips `.` before lookup).
    fn box_rel(&self, dest: &str) -> String {
        let joined = if dest.starts_with('/') {
            dest.to_string()
        } else {
            format!("{}/{}", self.workdir.trim_end_matches('/'), dest)
        };
        normalize_rel(&joined)
    }

    fn cmdline_vec(&self, c: &Cmdline) -> Vec<String> {
        match c {
            Cmdline::Shell(s) => {
                let mut v = self.shell.clone();
                v.push(expand(s, &self.vars));
                v
            }
            Cmdline::Exec(args) => args.iter().map(|a| expand(a, &self.vars)).collect(),
        }
    }

    fn set_env(&mut self, k: &str, v: &str) {
        self.vars.insert(k.to_string(), v.to_string());
        if let Some(slot) = self.env.iter_mut().find(|(ek, _)| ek == k) {
            slot.1 = v.to_string();
        } else {
            self.env.push((k.to_string(), v.to_string()));
        }
    }

    fn reset_config(&mut self) {
        self.env.clear();
        self.workdir = "/".to_string();
        self.user = None;
        self.cmd = None;
        self.entrypoint = None;
        self.labels.clear();
        self.exposed.clear();
        self.volumes.clear();
        self.shell = vec!["/bin/sh".to_string(), "-c".to_string()];
        self.shell_set = false;
        self.stopsignal = None;
        self.onbuild.clear();
        self.healthcheck = None;
        self.healthcheck_raw = None;
    }

    fn seed_config_from(&mut self, id: i64) {
        let meta = crate::discover::box_meta(id);
        let Some(j) = meta.get("oci_config") else { return; };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(j) else { return; };
        // Seed the base image's history as our prefix so the built image's
        // `history` is base steps + this build's steps.
        if let Some(h) = v.get("history").and_then(|x| x.as_array()) {
            self.history = h.clone();
        }
        let Some(inner) = v.get("config") else { return; };
        if let Some(env) = inner.get("Env").and_then(|e| e.as_array()) {
            for e in env {
                if let Some(s) = e.as_str() {
                    if let Some((k, val)) = s.split_once('=') {
                        self.env.push((k.to_string(), val.to_string()));
                        self.vars.insert(k.to_string(), val.to_string());
                    }
                }
            }
        }
        if let Some(w) = inner.get("WorkingDir").and_then(|x| x.as_str()) {
            if !w.is_empty() { self.workdir = w.to_string(); }
        }
        if let Some(u) = inner.get("User").and_then(|x| x.as_str()) {
            if !u.is_empty() { self.user = Some(u.to_string()); }
        }
        if let Some(c) = inner.get("Cmd").and_then(|x| x.as_array()) {
            self.cmd = Some(c.iter().filter_map(|v| v.as_str().map(String::from)).collect());
        }
        if let Some(e) = inner.get("Entrypoint").and_then(|x| x.as_array()) {
            self.entrypoint = Some(e.iter().filter_map(|v| v.as_str().map(String::from)).collect());
        }
        if let Some(sh) = inner.get("Shell").and_then(|x| x.as_array()) {
            let v: Vec<String> = sh.iter().filter_map(|x| x.as_str().map(String::from)).collect();
            if !v.is_empty() { self.shell = v; self.shell_set = true; }
        }
        if let Some(s) = inner.get("StopSignal").and_then(|x| x.as_str()) {
            if !s.is_empty() { self.stopsignal = Some(s.to_string()); }
        }
        if let Some(ob) = inner.get("OnBuild").and_then(|x| x.as_array()) {
            self.onbuild = ob.iter().filter_map(|x| x.as_str().map(String::from)).collect();
        }
        // Healthcheck is carried as-is from the base (already in nanosecond
        // form) when this build doesn't override it. Stored back verbatim in
        // config_json via the raw JSON value.
        if let Some(hc) = inner.get("Healthcheck") {
            self.healthcheck_raw = Some(hc.clone());
        }
    }

    fn config_json(&self) -> String {
        let env: Vec<String> = self.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let mut cfg = serde_json::Map::new();
        cfg.insert("Env".into(), serde_json::json!(env));
        cfg.insert("WorkingDir".into(), serde_json::json!(self.workdir));
        if let Some(u) = &self.user { cfg.insert("User".into(), serde_json::json!(u)); }
        if let Some(c) = &self.cmd { cfg.insert("Cmd".into(), serde_json::json!(c)); }
        if let Some(e) = &self.entrypoint { cfg.insert("Entrypoint".into(), serde_json::json!(e)); }
        if !self.labels.is_empty() {
            let m: serde_json::Map<String, serde_json::Value> = self.labels.iter()
                .map(|(k, v)| (k.clone(), serde_json::json!(v))).collect();
            cfg.insert("Labels".into(), serde_json::Value::Object(m));
        }
        if !self.exposed.is_empty() {
            let m: serde_json::Map<String, serde_json::Value> = self.exposed.iter()
                .map(|p| (p.clone(), serde_json::json!({}))).collect();
            cfg.insert("ExposedPorts".into(), serde_json::Value::Object(m));
        }
        if !self.volumes.is_empty() {
            let m: serde_json::Map<String, serde_json::Value> = self.volumes.iter()
                .map(|p| (p.clone(), serde_json::json!({}))).collect();
            cfg.insert("Volumes".into(), serde_json::Value::Object(m));
        }
        if self.shell_set {
            cfg.insert("Shell".into(), serde_json::json!(self.shell));
        }
        if let Some(s) = &self.stopsignal {
            cfg.insert("StopSignal".into(), serde_json::json!(s));
        }
        if !self.onbuild.is_empty() {
            cfg.insert("OnBuild".into(), serde_json::json!(self.onbuild));
        }
        if let Some(spec) = &self.healthcheck {
            cfg.insert("Healthcheck".into(), healthcheck_json(spec));
        } else if let Some(raw) = &self.healthcheck_raw {
            cfg.insert("Healthcheck".into(), raw.clone());
        }
        let mut top = serde_json::Map::new();
        top.insert("config".into(), serde_json::Value::Object(cfg));
        if !self.history.is_empty() {
            top.insert("history".into(), serde_json::json!(self.history));
        }
        serde_json::Value::Object(top).to_string()
    }

    fn stamp(&self, id: i64) {
        if let Ok(bx) = BoxState::create(id) {
            bx.set_meta("oci_config", &self.config_json());
        }
    }

    fn stamp_current(&self) {
        if let Some(c) = self.current { self.stamp(c); }
    }

    /// Stamp each build box's accumulated directive list as its `frame` meta.
    /// A box's COPY hint (longest common path of its own writes), when present,
    /// rides along so the Dockerfile emitter can surface a commented `#COPY`.
    fn stamp_frames(&self) {
        for (id, dirs) in &self.frames {
            if let Ok(bx) = BoxState::create(*id) {
                let mut frame = serde_json::json!({"directives": dirs});
                if let Some(h) = self.copy_hints.get(id) {
                    if !h.is_empty() {
                        frame["copy_hint"] = serde_json::Value::String(h.clone());
                    }
                }
                bx.set_meta("frame", &frame.to_string());
            }
        }
    }

    fn finish(&mut self, tag: Option<String>) -> Result<()> {
        // Close the final stage's AS name too, so `FROM x AS final` is addressable.
        if let (Some(name), Some(cur)) = (self.pending_stage.take(), self.current) {
            self.stages.insert(name, cur);
        }
        let top = self.current.ok_or_else(|| anyhow!("empty build (no FROM)"))?;
        self.stamp(top);
        self.stamp_frames();
        let name = if let Some(t) = tag {
            let bx = BoxState::create(top)?;
            bx.set_meta("name", &t);
            bx.set_meta("oci_reference", &t);
            t
        } else {
            format!("B{top}")
        };
        println!("built image '{name}' → top box {top}");
        Ok(())
    }
}

/// Render a parsed HEALTHCHECK into the OCI image-config `Healthcheck` object.
/// Durations are stored as nanosecond ints (Go `time.Duration`), the `Test`
/// array uses Docker's `CMD`/`CMD-SHELL`/`NONE` leading tokens.
fn healthcheck_json(spec: &crate::dockerfile::HealthcheckSpec) -> serde_json::Value {
    use crate::dockerfile::Cmdline;
    if spec.none {
        return serde_json::json!({ "Test": ["NONE"] });
    }
    let test: Vec<String> = match &spec.test {
        Some(Cmdline::Exec(args)) => {
            let mut v = vec!["CMD".to_string()];
            v.extend(args.iter().cloned());
            v
        }
        Some(Cmdline::Shell(s)) => vec!["CMD-SHELL".to_string(), s.clone()],
        None => vec!["NONE".to_string()],
    };
    let mut m = serde_json::Map::new();
    m.insert("Test".into(), serde_json::json!(test));
    let mut put_dur = |key: &str, raw: &Option<String>| {
        if let Some(d) = raw.as_ref().and_then(|s| parse_duration_ns(s)) {
            m.insert(key.into(), serde_json::json!(d));
        }
    };
    put_dur("Interval", &spec.interval);
    put_dur("Timeout", &spec.timeout);
    put_dur("StartPeriod", &spec.start_period);
    put_dur("StartInterval", &spec.start_interval);
    if let Some(r) = spec.retries {
        m.insert("Retries".into(), serde_json::json!(r));
    }
    serde_json::Value::Object(m)
}

/// Parse a Go-style duration (`30s`, `1m30s`, `500ms`, `1h`) into nanoseconds.
/// Supports unit suffixes ns/us/µs/ms/s/m/h and concatenated terms. Returns
/// None on any malformed input so the caller simply omits the field.
fn parse_duration_ns(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() { return None; }
    let bytes = s.as_bytes();
    let mut i = 0usize;
    let mut total: i64 = 0;
    let mut saw_term = false;
    while i < bytes.len() {
        // number (digits + optional fraction)
        let num_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') { i += 1; }
        if i == num_start { return None; }
        let val: f64 = s[num_start..i].parse().ok()?;
        // unit
        let unit_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_digit() && bytes[i] != b'.' { i += 1; }
        let unit = &s[unit_start..i];
        let scale: f64 = match unit {
            "ns" => 1.0,
            "us" | "µs" | "μs" => 1_000.0,
            "ms" => 1_000_000.0,
            "s" => 1_000_000_000.0,
            "m" => 60_000_000_000.0,
            "h" => 3_600_000_000_000.0,
            _ => return None,
        };
        total = total.checked_add((val * scale) as i64)?;
        saw_term = true;
    }
    if saw_term { Some(total) } else { None }
}

/// Collapse a slash path to a clean box-relative path: drop empty and `.`
/// components, pop on `..`, strip the leading slash.
fn normalize_rel(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for comp in p.split('/') {
        match comp {
            "" | "." => {}
            ".." => { out.pop(); }
            c => out.push(c),
        }
    }
    out.join("/")
}

/// Parse a `--chown` value. v1 accepts numeric `uid[:gid]` only (name lookups
/// would need the box's /etc/passwd). Returns None for anything non-numeric so
/// the caller leaves ownership at the ingested default.
fn parse_chown(s: &str) -> Option<(u32, u32)> {
    let (u, g) = match s.split_once(':') {
        Some((u, g)) => (u, g),
        None => (s, s),
    };
    Some((u.trim().parse().ok()?, g.trim().parse().ok()?))
}

/// Mint a box id above every existing box — at-rest sqlars AND live backing
/// dirs — so a build that runs alongside a live engine never reuses an id the
/// engine just handed out. Mirrors the engine's own `max(at_rest, live)+1`.
fn mint_box_id() -> i64 {
    let mut max = 0i64;
    for dir in [paths::state_home(), paths::live_home()] {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for ent in rd.flatten() {
                let p = ent.path();
                let stem = p.file_stem().and_then(|s| s.to_str());
                if let Some(n) = stem.and_then(|s| s.parse::<i64>().ok()) {
                    if n > max { max = n; }
                }
            }
        }
    }
    max + 1
}

/// Box id of the child of `parent` named `name`, polling briefly: the engine
/// finalizes a RUN box's teardown just after our `runner::run` returns, so the
/// name may take a beat to appear in discovery.
fn find_child_named(parent: i64, name: &str) -> Option<i64> {
    for _ in 0..50 {
        let boxes = crate::discover::discover();
        if let Some(b) = boxes.values()
            .find(|b| b.parent == Some(parent) && b.name == name) {
            return Some(b.box_id);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    None
}

/// Create `rel` and every ancestor as mode-0755 dirs (no-op for ones that exist).
fn ensure_dir_chain(b: &BoxState, rel: &str) {
    let mut acc = String::new();
    for comp in rel.split('/') {
        if comp.is_empty() { continue; }
        if acc.is_empty() { acc.push_str(comp); } else { acc.push('/'); acc.push_str(comp); }
        b.set_dir(&acc, 0o755, 0);
    }
}

/// Copy a single host file into box `b` at `target_rel`, creating ancestors.
fn copy_file(b: &BoxState, src: &Path, target_rel: &str,
             owner: Option<(u32, u32)>, mode_override: Option<u32>) -> Result<()> {
    if let Some(parent) = Path::new(target_rel).parent().and_then(|p| p.to_str()) {
        if !parent.is_empty() { ensure_dir_chain(b, parent); }
    }
    let meta = std::fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        let tgt = std::fs::read_link(src)?;
        b.set_symlink(target_rel, &tgt, 0);
        if let Some((u, g)) = owner { b.set_owner(target_rel, u, g); }
        return Ok(());
    }
    let mode = mode_override.unwrap_or_else(|| meta.permissions().mode() & 0o7777);
    let rowid = b.ensure_file_row(target_rel, 0o100000 | mode, 0);
    let bp = crate::depot::blob_path(b.id, rowid);
    if let Some(p) = bp.parent() { std::fs::create_dir_all(p)?; }
    let sz = std::fs::copy(src, &bp)
        .with_context(|| format!("copy {} → blob", src.display()))?;
    let _ = std::fs::set_permissions(&bp, std::fs::Permissions::from_mode(mode));
    let mtime_ns = meta.modified().ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64).unwrap_or(0);
    b.finalize_file(target_rel, sz as i64, mtime_ns, 0);
    if let Some((u, g)) = owner { b.set_owner(target_rel, u, g); }
    Ok(())
}

/// Recursively copy the CONTENTS of host dir `src` into box `b` under `dst_rel`
/// (Docker COPY-dir semantics: the directory's children land in dest).
fn copy_tree(b: &BoxState, src: &Path, dst_rel: &str,
             owner: Option<(u32, u32)>, mode_override: Option<u32>) -> Result<()> {
    if !dst_rel.is_empty() { ensure_dir_chain(b, dst_rel); }
    for ent in std::fs::read_dir(src)? {
        let ent = ent?;
        let name = ent.file_name();
        let name = name.to_string_lossy();
        let child = ent.path();
        let target = if dst_rel.is_empty() { name.to_string() }
                     else { format!("{dst_rel}/{name}") };
        let ft = ent.file_type()?;
        if ft.is_dir() {
            b.set_dir(&target, 0o755, 0);
            copy_tree(b, &child, &target, owner, mode_override)?;
        } else {
            copy_file(b, &child, &target, owner, mode_override)?;
        }
    }
    Ok(())
}

/// Recursively copy a directory from a SOURCE box's merged view (`ov`,`src_id`,
/// `src_rel`) into box `b` under `dst_rel` — the box-view analogue of
/// copy_tree, used by `COPY --from`. Docker copies the source dir's CONTENTS
/// into dest.
fn copy_box_tree(ov: &crate::overlay::Overlay, src_id: i64, src_rel: &str,
                 b: &BoxState, dst_rel: &str, owner: Option<(u32, u32)>,
                 mode: Option<u32>) -> Result<()> {
    if !dst_rel.is_empty() { ensure_dir_chain(b, dst_rel); }
    let entries = ov.box_list_dir(src_id, src_rel)
        .map_err(|e| anyhow!("list '{src_rel}' in stage: {e}"))?;
    for (name, kind) in entries {
        let child_src = if src_rel.is_empty() { name.clone() }
                        else { format!("{src_rel}/{name}") };
        let child_dst = if dst_rel.is_empty() { name.clone() }
                        else { format!("{dst_rel}/{name}") };
        match kind {
            'd' => {
                b.set_dir(&child_dst, 0o755, 0);
                copy_box_tree(ov, src_id, &child_src, b, &child_dst, owner, mode)?;
            }
            'l' => {
                let tgt = ov.box_read_file(src_id, &child_src)
                    .map_err(|e| anyhow!("readlink '{child_src}': {e}"))?;
                b.set_symlink(&child_dst,
                    Path::new(&String::from_utf8_lossy(&tgt).into_owned()), 0);
                if let Some((u, g)) = owner { b.set_owner(&child_dst, u, g); }
            }
            _ => {
                let bytes = ov.box_read_file(src_id, &child_src)
                    .map_err(|e| anyhow!("read '{child_src}': {e}"))?;
                let m = mode.or_else(|| ov.box_file_mode(src_id, &child_src))
                    .unwrap_or(0o644);
                put_file_bytes(b, &child_dst, &bytes, m, now_ns())?;
                if let Some((u, g)) = owner { b.set_owner(&child_dst, u, g); }
            }
        }
    }
    Ok(())
}

/// Write `bytes` into box `b` at `target_rel` as a regular file with `mode`,
/// creating ancestor dirs. The box-bytes analogue of copy_file (used by
/// ADD-of-URL, ADD-tar-extract, and COPY --from).
fn put_file_bytes(b: &BoxState, target_rel: &str, bytes: &[u8], mode: u32,
                  mtime_ns: i64) -> Result<()> {
    if let Some(parent) = Path::new(target_rel).parent().and_then(|p| p.to_str()) {
        if !parent.is_empty() { ensure_dir_chain(b, parent); }
    }
    let m = mode & 0o7777;
    let rowid = b.ensure_file_row(target_rel, 0o100000 | m, 0);
    let bp = crate::depot::blob_path(b.id, rowid);
    if let Some(p) = bp.parent() { std::fs::create_dir_all(p)?; }
    std::fs::write(&bp, bytes)
        .with_context(|| format!("write blob for {target_rel}"))?;
    let _ = std::fs::set_permissions(&bp, std::fs::Permissions::from_mode(m));
    b.finalize_file(target_rel, bytes.len() as i64, mtime_ns, 0);
    Ok(())
}

/// Join a dest dir/file path with a source leaf name. When `dst_is_dir` the
/// leaf is appended; otherwise the dest IS the target path (single-file rename).
fn join_dest(dst_rel: &str, dst_is_dir: bool, base: &str) -> String {
    if dst_is_dir || dst_rel.is_empty() {
        if dst_rel.is_empty() { base.to_string() } else { format!("{dst_rel}/{base}") }
    } else {
        dst_rel.to_string()
    }
}

/// Last path component of a box-relative path (the file/dir basename).
fn leaf_name(rel: &str) -> String {
    rel.rsplit('/').next().unwrap_or(rel).to_string()
}

/// Final path segment of a URL (its filename), query/fragment stripped.
fn url_basename(url: &str) -> String {
    let no_q = url.split(['?', '#']).next().unwrap_or(url);
    no_q.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string()
}

fn now_ns() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64).unwrap_or(0)
}

/// What `archive_kind` decided a local ADD source is.
enum ArchiveKind {
    /// A tar stream (plain, gzip, zstd, xz, or bzip2) — auto-extract it.
    Tar,
    /// Not an archive — ADD copies it as a plain file (like COPY).
    NotArchive,
}

/// Sniff a local ADD source's header: Docker auto-extracts a source it
/// recognizes as a (optionally compressed) tar. We decode gzip/zstd/xz/bzip2/
/// plain — Docker's full set — so a tarball the user expected extracted never
/// gets silently plain-copied.
fn archive_kind(path: &Path) -> Result<ArchiveKind> {
    let mut f = File::open(path)?;
    let mut head = [0u8; 512];
    let mut n = 0usize;
    loop {
        match f.read(&mut head[n..]) {
            Ok(0) => break,
            Ok(k) => { n += k; if n == head.len() { break; } }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let h = &head[..n];
    if sniff(h).is_some() {
        // gzip / zstd / xz / bzip2 magic → a compressed tar (Docker assumes so).
        return Ok(ArchiveKind::Tar);
    }
    // Plain tar: the ustar magic lives at offset 257.
    if h.len() >= 262 && &h[257..262] == b"ustar" {
        return Ok(ArchiveKind::Tar);
    }
    Ok(ArchiveKind::NotArchive)
}

/// Extract a (optionally compressed) tar `blob` INTO box `b` under `dst_rel`,
/// the ADD-of-local-tar path. Plain extraction — no AUFS whiteout convention
/// (a build-context tarball never carries `.wh.` entries). Files/dirs/symlinks
/// are handled; `--chown`/`--chmod` overrides win over the tar's own metadata.
fn extract_tar_into(b: &BoxState, blob: &[u8], dst_rel: &str,
                    owner: Option<(u32, u32)>, mode_override: Option<u32>) -> Result<()> {
    use std::path::Component;
    if !dst_rel.is_empty() { ensure_dir_chain(b, dst_rel); }
    let reader = decompressor(blob, "")?;
    let mut ar = tar::Archive::new(reader);
    for entry in ar.entries().context("read tar entries")? {
        let mut e = entry.context("tar entry")?;
        let raw = match e.path() { Ok(p) => p.into_owned(), Err(_) => continue };
        if raw.is_absolute()
            || raw.components().any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
        { continue; }
        let rel_s = raw.to_string_lossy();
        let rel = rel_s.trim_end_matches('/');
        if rel.is_empty() { continue; }
        let target = if dst_rel.is_empty() { rel.to_string() }
                     else { format!("{dst_rel}/{rel}") };
        let (tmode, mtime_ns, uid, gid, et) = {
            let h = e.header();
            (
                h.mode().unwrap_or(0o644) & 0o7777,
                (h.mtime().unwrap_or(0) as i64).saturating_mul(1_000_000_000),
                h.uid().unwrap_or(0) as u32,
                h.gid().unwrap_or(0) as u32,
                h.entry_type(),
            )
        };
        let mode = mode_override.unwrap_or(tmode);
        match et {
            tar::EntryType::Directory => {
                if let Some(p) = Path::new(&target).parent().and_then(|p| p.to_str()) {
                    if !p.is_empty() { ensure_dir_chain(b, p); }
                }
                b.set_dir(&target, mode, 0);
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let mut body = Vec::new();
                e.read_to_end(&mut body).context("read tar file body")?;
                put_file_bytes(b, &target, &body, mode, mtime_ns)?;
            }
            tar::EntryType::Symlink => {
                if let Ok(Some(link)) = e.link_name() {
                    if let Some(p) = Path::new(&target).parent().and_then(|p| p.to_str()) {
                        if !p.is_empty() { ensure_dir_chain(b, p); }
                    }
                    b.set_symlink(&target, &link, 0);
                }
            }
            // Hardlinks/devices/fifos in an ADD tarball are rare; skip rather
            // than guess. (Layer ingest handles the full set; ADD does not.)
            _ => continue,
        }
        let (u, g) = owner.unwrap_or((uid, gid));
        if u != 0 || g != 0 { b.set_owner(&target, u, g); }
    }
    Ok(())
}

/// Fetch a URL into memory for `ADD <url>`. Uses reqwest (already in the tree
/// via oci-client) with platform TLS verification + redirect following.
fn fetch_url(url: &str) -> Result<Vec<u8>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all().build().context("build runtime")?;
    rt.block_on(async {
        let resp = reqwest::get(url).await.context("GET")?;
        let status = resp.status();
        if !status.is_success() {
            bail!("server returned HTTP {status}");
        }
        Ok(resp.bytes().await.context("read body")?.to_vec())
    })
}

pub(crate) struct LoadOutcome {
    pub base_id: i64,
    pub base_name: String,
    pub top_id: i64,
    pub top_name: String,
    pub n_layers: usize,
    /// True when a cosign trust policy covered this reference and the image's
    /// signature verified. Surfaced so the CLI can report it on the user's
    /// terminal (the load itself runs host-side in the engine).
    pub verified: bool,
}

/// One pulled layer ready to ingest. media_type drives the decompressor.
struct PulledLayer {
    media_type: String,
    bytes: Vec<u8>,
    digest: String,
}

/// Result of fetching: layers in BOTTOM→TOP order plus the image config.
struct PulledImage {
    layers: Vec<PulledLayer>,
    config_json: String,
    reference: String,
    /// The image's manifest digest (`sha256:…`). The image-cache v2 key: two
    /// references that resolve to the same manifest coalesce onto one loaded
    /// stack, and a `:tag` that has moved (new manifest) re-pulls. Empty when
    /// the source didn't surface one.
    manifest_digest: String,
    /// cosign signatures discovered alongside the image (oci-archive/oci-layout
    /// index, or the registry `.sig` tag). Verified in `load` when the trust
    /// policy requires it; otherwise unused.
    signatures: Vec<crate::oci_verify::CosignSig>,
}

async fn load(reference: &str, name: Option<String>) -> Result<LoadOutcome> {
    let mut image = fetch(reference).await?;
    // Record the reference the USER gave: `fetch` rewrites an archive ref to a
    // temp-layout path, but the image cache (find_loaded_by_reference) dedups on
    // the original string, so normalize it back here.
    image.reference = reference.to_string();
    // Key-based cosign verification (host-side; keys never enter a box). When the
    // trust policy covers this reference, the image MUST carry a valid signature
    // for its manifest digest under the configured key, or the load fails closed.
    let policy = crate::oci_verify::Policy::load();
    let verified = if let Some(key) = policy.key_for(reference) {
        crate::oci_verify::verify(key, &image.manifest_digest, &image.signatures)
            .map_err(|e| anyhow!("cosign verification failed for '{reference}': {e}"))?;
        true
    } else {
        false
    };
    let mut outcome = install_chain(image, name)?;
    outcome.verified = verified;
    Ok(outcome)
}

// ── reference resolution ────────────────────────────────────────────────────

async fn fetch(reference: &str) -> Result<PulledImage> {
    if let Some(path) = reference.strip_prefix("oci-archive:") {
        return fetch_oci_archive(Path::new(path));
    }
    if let Some(path) = reference.strip_prefix("oci-layout:") {
        return fetch_oci_layout(Path::new(path));
    }
    fetch_registry(reference).await
}

async fn fetch_registry(reference: &str) -> Result<PulledImage> {
    // Respect the host's /etc/containers/registries.conf: short-name
    // aliases, unqualified-search-registries, and location/mirror/blocked
    // remaps. Candidates are tried in order (mirrors before their primary)
    // so a policy-mandated local mirror is used without the user having to
    // spell it out. With no config this yields exactly the old behavior
    // (docker.io + library/ via the oci-client defaults).
    let resolved = crate::containers_conf::ContainersConf::load()
        .resolve(reference);
    if let Some(msg) = resolved.blocked {
        bail!("{msg}");
    }
    let mut last_err: Option<anyhow::Error> = None;
    for cand in &resolved.candidates {
        match pull_one(&cand.reference).await {
            Ok(img) => {
                if cand.reference != reference && !cand.via.is_empty() {
                    eprintln!("sarun oci: '{reference}' → '{}' ({})",
                              cand.reference, cand.via);
                }
                return Ok(img);
            }
            Err(e) => {
                let e = e.context(format!("pull '{}'{}", cand.reference,
                    if cand.via.is_empty() { String::new() }
                    else { format!(" ({})", cand.via) }));
                last_err = Some(match last_err {
                    Some(prev) => e.context(format!("{prev:#}")),
                    None => e,
                });
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("no pull candidates for '{reference}'")))
}

async fn pull_one(reference: &str) -> Result<PulledImage> {
    let r = Reference::from_str(reference)
        .with_context(|| format!("parse reference '{reference}'"))?;
    let client = Client::new(ClientConfig::default());
    // Accepted media types: docker + oci variants in gzip / uncompressed /
    // zstd. Passing them all so a server that has multiple variants doesn't
    // serve us a zstd we wouldn't decode.
    let accepted = vec![
        IMAGE_LAYER_GZIP_MEDIA_TYPE,
        IMAGE_LAYER_MEDIA_TYPE,
        ZSTD_LAYER_MEDIA_TYPE,
        DOCKER_LAYER_GZIP_MEDIA_TYPE,
    ];
    let auth = registry_auth_for(reference);
    let img = client.pull(&r, &auth, accepted).await
        .context("pull image")?;
    let mut layers = Vec::with_capacity(img.layers.len());
    for layer in img.layers {
        let ImageLayer { data, media_type, .. } = layer;
        let bytes = data.to_vec();
        let digest = digest_of(&bytes);
        layers.push(PulledLayer { media_type, bytes, digest });
    }
    let config_json = String::from_utf8(img.config.data.to_vec())
        .context("config is not utf-8")?;
    let manifest_digest = img.digest.clone().unwrap_or_default();
    // Best-effort: fetch any cosign `.sig` artifact for this digest (only used
    // if the trust policy requires verification — then absence fails closed).
    let signatures = fetch_registry_sigs(&r, &auth, &manifest_digest).await;
    Ok(PulledImage {
        layers,
        config_json,
        reference: reference.to_string(),
        // oci-client surfaces the manifest digest it pulled; the cache keys on it.
        manifest_digest,
        signatures,
    })
}

/// Best-effort fetch of the cosign `.sig` artifact for `manifest_digest` from
/// the same registry/repo (`<repo>:sha256-<hex>.sig`). Returns the signatures it
/// finds; any error (no signature, network, auth) yields an empty list, which
/// makes a policy-required verification fail closed rather than pass.
async fn fetch_registry_sigs(r: &Reference, auth: &RegistryAuth, manifest_digest: &str)
    -> Vec<crate::oci_verify::CosignSig> {
    let dhex = manifest_digest.strip_prefix("sha256:").unwrap_or(manifest_digest);
    if dhex.is_empty() { return vec![]; }
    let sig_ref_str = format!("{}/{}:sha256-{}.sig", r.registry(), r.repository(), dhex);
    let Ok(sig_ref) = Reference::from_str(&sig_ref_str) else { return vec![]; };
    let client = Client::new(ClientConfig::default());
    let accepted = vec!["application/vnd.dev.cosign.simplesigning.v1+json"];
    let Ok(img) = client.pull(&sig_ref, auth, accepted).await else { return vec![]; };
    let mut out = vec![];
    for layer in img.layers {
        if let Some(sig) = layer.annotations.as_ref()
            .and_then(|a| a.get("dev.cosignproject.cosign/signature")) {
            out.push(crate::oci_verify::CosignSig {
                payload: layer.data.to_vec(),
                signature_b64: sig.clone(),
            });
        }
    }
    out
}

/// Resolve registry credentials for `reference` from the host's Docker config
/// (`$DOCKER_CONFIG/config.json` or `~/.docker/config.json`) plus credential
/// helpers. Returns `Anonymous` when nothing is configured for the registry, so
/// public pulls are unchanged. Credentials are read HOST-side here in the
/// engine/CLI and never enter a box.
fn registry_auth_for(reference: &str) -> RegistryAuth {
    let Ok(r) = Reference::from_str(reference) else { return RegistryAuth::Anonymous; };
    let host = r.registry().to_string();
    // Docker Hub creds live under a legacy key in config.json.
    let mut keys = vec![host.clone()];
    if matches!(host.as_str(),
                "docker.io" | "registry-1.docker.io" | "index.docker.io") {
        keys.push("https://index.docker.io/v1/".to_string());
    }
    let Some(cfg) = read_docker_config() else { return RegistryAuth::Anonymous; };
    // 1. A direct auths[<key>] entry: base64 "user:pass", or username/password.
    if let Some(auths) = cfg.get("auths").and_then(|v| v.as_object()) {
        for k in &keys {
            let Some(entry) = auths.get(k).and_then(|v| v.as_object()) else { continue };
            if let Some(b64) = entry.get("auth").and_then(|v| v.as_str()) {
                if let Some((u, p)) = decode_basic_auth(b64) {
                    return RegistryAuth::Basic(u, p);
                }
            }
            if let (Some(u), Some(p)) = (entry.get("username").and_then(|v| v.as_str()),
                                         entry.get("password").and_then(|v| v.as_str())) {
                return RegistryAuth::Basic(u.to_string(), p.to_string());
            }
        }
    }
    // 2. A credential helper (per-registry credHelpers beats the global credsStore).
    let helper = cfg.get("credHelpers").and_then(|v| v.as_object())
        .and_then(|m| keys.iter().find_map(|k| m.get(k)).and_then(|v| v.as_str()))
        .map(String::from)
        .or_else(|| cfg.get("credsStore").and_then(|v| v.as_str()).map(String::from));
    if let Some(helper) = helper {
        if let Some(auth) = credential_helper_get(&helper, &host) {
            return auth;
        }
    }
    RegistryAuth::Anonymous
}

fn read_docker_config() -> Option<serde_json::Value> {
    let path = std::env::var_os("DOCKER_CONFIG")
        .map(|d| PathBuf::from(d).join("config.json"))
        .or_else(|| std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".docker").join("config.json")))?;
    serde_json::from_slice(&std::fs::read(&path).ok()?).ok()
}

fn decode_basic_auth(b64: &str) -> Option<(String, String)> {
    use base64::{Engine as _, prelude::BASE64_STANDARD};
    let raw = BASE64_STANDARD.decode(b64.trim()).ok()?;
    let s = String::from_utf8(raw).ok()?;
    let (u, p) = s.split_once(':')?;
    Some((u.to_string(), p.to_string()))
}

/// Run `docker-credential-<helper> get` with the server on stdin; parse the
/// `{Username, Secret}` reply into Basic auth. An identity-token result
/// (`Username == "<token>"`) can't be expressed as Basic, so we skip it.
fn credential_helper_get(helper: &str, server: &str) -> Option<RegistryAuth> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new(format!("docker-credential-{helper}"))
        .arg("get")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
        .spawn().ok()?;
    child.stdin.take()?.write_all(server.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() { return None; }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let user = v.get("Username").and_then(|x| x.as_str())?;
    let secret = v.get("Secret").and_then(|x| x.as_str())?;
    if user == "<token>" { return None; }
    Some(RegistryAuth::Basic(user.to_string(), secret.to_string()))
}

fn fetch_oci_layout(path: &Path) -> Result<PulledImage> {
    // OCI image-layout: a directory with index.json + oci-layout + blobs/sha256/.
    // For v1 take the FIRST manifest in index.json; multi-arch indexes pick
    // the first descriptor that points at a manifest (not another index).
    let index_path = path.join("index.json");
    let idx_bytes = std::fs::read(&index_path)
        .with_context(|| format!("read {}", index_path.display()))?;
    let manifest_digest = index_manifest_digest(&idx_bytes)?;
    let manifest_bytes = read_blob_by_digest(path, &manifest_digest)?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)
        .context("parse manifest")?;
    let config_desc = manifest.get("config")
        .ok_or_else(|| anyhow!("manifest has no config"))?;
    let config_digest = config_desc.get("digest").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("config has no digest"))?;
    let config_bytes = read_blob_by_digest(path, config_digest)?;
    let config_json = String::from_utf8(config_bytes)
        .context("config is not utf-8")?;
    let layers_desc = manifest.get("layers").and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("manifest has no layers"))?;
    let mut layers = Vec::with_capacity(layers_desc.len());
    for l in layers_desc {
        let digest = l.get("digest").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("layer has no digest"))?;
        let media_type = l.get("mediaType").and_then(|v| v.as_str())
            .unwrap_or(IMAGE_LAYER_GZIP_MEDIA_TYPE).to_string();
        let bytes = read_blob_by_digest(path, digest)?;
        layers.push(PulledLayer {
            media_type, bytes,
            digest: digest.to_string(),
        });
    }
    let idx: Value = serde_json::from_slice(&idx_bytes).unwrap_or(Value::Null);
    let signatures = cosign_sigs_from_layout(path, &idx, &manifest_digest);
    Ok(PulledImage {
        layers, config_json,
        reference: format!("oci-layout:{}", path.display()),
        manifest_digest,
        signatures,
    })
}

/// Discover cosign signatures for `manifest_digest` in an oci-layout: an index
/// manifest descriptor tagged (annotation `org.opencontainers.image.ref.name`)
/// `sha256-<hex>.sig`, whose layers carry the simple-signing payload (blob) and
/// the base64 signature (annotation `dev.cosignproject.cosign/signature`).
fn cosign_sigs_from_layout(path: &Path, idx: &Value, manifest_digest: &str)
    -> Vec<crate::oci_verify::CosignSig> {
    let dhex = manifest_digest.strip_prefix("sha256:").unwrap_or(manifest_digest);
    let want = format!("sha256-{dhex}.sig");
    let mut out = vec![];
    let Some(descs) = idx.get("manifests").and_then(|v| v.as_array()) else { return out };
    for d in descs {
        let name = d.get("annotations")
            .and_then(|a| a.get("org.opencontainers.image.ref.name"))
            .and_then(Value::as_str);
        if name != Some(want.as_str()) { continue; }
        let Some(sig_dg) = d.get("digest").and_then(Value::as_str) else { continue };
        let Ok(mbytes) = read_blob_by_digest(path, sig_dg) else { continue };
        let Ok(m) = serde_json::from_slice::<Value>(&mbytes) else { continue };
        for layer in m.get("layers").and_then(|v| v.as_array()).into_iter().flatten() {
            let sig = layer.get("annotations")
                .and_then(|a| a.get("dev.cosignproject.cosign/signature"))
                .and_then(Value::as_str);
            let blob_dg = layer.get("digest").and_then(Value::as_str);
            if let (Some(sig), Some(bd)) = (sig, blob_dg) {
                if let Ok(payload) = read_blob_by_digest(path, bd) {
                    out.push(crate::oci_verify::CosignSig {
                        payload, signature_b64: sig.to_string(),
                    });
                }
            }
        }
    }
    out
}

/// The platform-matched manifest descriptor digest from an OCI `index.json`.
/// Shared by the layout loader and the cheap image-cache probe so both agree
/// on what "this image's manifest digest" is.
fn index_manifest_digest(idx_bytes: &[u8]) -> Result<String> {
    let idx: serde_json::Value = serde_json::from_slice(idx_bytes)
        .context("parse index.json")?;
    let descs = idx.get("manifests").and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("index.json has no manifests array"))?;
    // Multi-arch index: prefer the host arch+os descriptor; fall back to the
    // first (single-arch / unannotated index) rather than failing.
    let (host_arch, host_os) = host_platform();
    descs.iter()
        .find(|d| matches_platform(d, &host_arch, &host_os))
        .or_else(|| descs.first())
        .and_then(|d| d.get("digest").and_then(|v| v.as_str()))
        .map(String::from)
        .ok_or_else(|| anyhow!("no manifest digest in index.json"))
}

fn fetch_oci_archive(path: &Path) -> Result<PulledImage> {
    // OCI image-archive: a tar of an oci-layout. Extract to a temp dir, then
    // fetch_oci_layout from there. Cheap given image sizes (an alpine layer
    // is ~3 MB; the archive itself is what the user already had on disk).
    let tmp = tempdir_for_archive()?;
    let file = File::open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let mut ar = tar::Archive::new(file);
    ar.unpack(&tmp).context("untar oci-archive")?;
    fetch_oci_layout(&tmp)
}

fn tempdir_for_archive() -> Result<PathBuf> {
    let base = paths::runtime_home().join("oci-archive-tmp");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base)?;
    Ok(base)
}

/// Host platform tuple in OCI naming. The OS is always "linux" on this
/// engine; the arch maps Rust's target_arch to OCI's name (amd64/arm64/etc.).
fn host_platform() -> (String, String) {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "arm" => "arm",
        "powerpc64" => "ppc64le",
        "riscv64" => "riscv64",
        "s390x" => "s390x",
        other => other,
    };
    (arch.to_string(), "linux".to_string())
}

/// Does an index manifest descriptor's platform field match the host?
/// A descriptor without a platform field is treated as a NON-match (so the
/// host-arch entry, if present, is preferred). The first-fallback caller
/// handles the no-match-but-some-entry case.
fn matches_platform(desc: &serde_json::Value, arch: &str, os: &str) -> bool {
    let Some(p) = desc.get("platform") else { return false; };
    p.get("architecture").and_then(|v| v.as_str()) == Some(arch)
        && p.get("os").and_then(|v| v.as_str()) == Some(os)
}

fn read_blob_by_digest(layout: &Path, digest: &str) -> Result<Vec<u8>> {
    // digest is "sha256:<hex>"; the blob lives at blobs/sha256/<hex>.
    let (algo, hex) = digest.split_once(':')
        .ok_or_else(|| anyhow!("malformed digest '{digest}'"))?;
    // A manifest-supplied digest is untrusted: both components index into a host
    // path, so reject anything that isn't a bare path segment or that could
    // traverse out of the blob store (path separators, `..`, NUL, empties).
    let ok_segment = |s: &str| {
        !s.is_empty()
            && !s.contains('/')
            && !s.contains('\\')
            && !s.contains('\0')
            && s != "."
            && s != ".."
    };
    if !ok_segment(algo) || !ok_segment(hex) {
        bail!("refusing unsafe digest path component in '{digest}'");
    }
    let p = layout.join("blobs").join(algo).join(hex);
    let bytes = std::fs::read(&p)
        .with_context(|| format!("read blob {}", p.display()))?;
    // Content-addressable integrity: the bytes MUST hash to the digest the
    // descriptor/filename claims. Without this an oci-archive/oci-layout with a
    // corrupted or swapped blob is accepted silently. (Registry transfers are
    // verified inside oci-client.) We can hash sha256 — the OCI default; any
    // other algorithm passes through unverified rather than failing the load.
    if algo == "sha256" {
        let actual = sha256_hex(&bytes);
        if !actual.eq_ignore_ascii_case(hex) {
            bail!("blob digest mismatch: {} claims {digest} but hashes to \
                   sha256:{actual}", p.display());
        }
    }
    Ok(bytes)
}

/// Lowercase hex sha256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in d { s.push_str(&format!("{b:02x}")); }
    s
}

fn digest_of(bytes: &[u8]) -> String {
    // The real layer-blob digest (sha256 of the bytes we received). For the
    // registry path oci-client has already verified transfer integrity; we
    // record the digest so users can correlate sqlars to layers and so the
    // image cache has a real key. Archive/layout layers carry the manifest's
    // own digest instead (and read_blob_by_digest verifies it).
    format!("sha256:{}", sha256_hex(bytes))
}

// ── box chain construction ──────────────────────────────────────────────────

fn install_chain(image: PulledImage, base_name: Option<String>)
    -> Result<LoadOutcome>
{
    if image.layers.is_empty() {
        bail!("image has no layers");
    }
    let mut prev: Option<i64> = None;
    let mut base_id: i64 = 0;
    let mut base_name_out = String::new();
    let mut top_id: i64 = 0;
    let mut top_name = String::new();
    let n = image.layers.len();
    // mint_box_id() scans BOTH state_home (at-rest) and live_home (a live box's
    // backing dir, created by `register` at id-allocation time), so an id picked
    // here can't collide with a running box that has no at-rest sqlar yet. This
    // is the same allocator the build path uses, and it makes install_chain safe
    // to run engine-side (the OCI pull RPC), not just in a standalone CLI.
    let mut next_id = mint_box_id();
    let actual_base_name = base_name.clone();
    for (i, layer) in image.layers.into_iter().enumerate() {
        let id = next_id;
        next_id += 1;
        let is_base = i == 0;
        let is_top = i == n - 1;
        let name = if is_base {
            actual_base_name.clone().unwrap_or_else(|| format!("C{id}"))
        } else {
            format!("L{id}")
        };
        let b = BoxState::create(id)
            .with_context(|| format!("create sqlar for box {id}"))?;
        b.set_meta("name", &name);
        b.set_meta("oci_reference", &image.reference);
        if !image.manifest_digest.is_empty() {
            b.set_meta("oci_manifest_digest", &image.manifest_digest);
        }
        b.set_meta("oci_layer_digest", &layer.digest);
        b.set_meta("oci_layer_index", &i.to_string());
        if is_base {
            b.set_no_host_fallback(true);
            b.set_meta("no_host_fallback", "1");
        }
        if let Some(p) = prev {
            b.set_parent(Some(p));
            b.set_meta("parent_box_id", &p.to_string());
        }
        if is_top {
            // Stash the image config on the TOP layer so a future runner can
            // pick up env/cmd/entrypoint/workdir/user. Storing the verbatim
            // JSON keeps us compatible with oci_spec::image::ImageConfiguration
            // (parse on demand) without committing to a schema here.
            b.set_meta("oci_config", &image.config_json);
        }
        ingest_layer(&b, &layer.bytes, &layer.media_type)
            .with_context(|| format!("ingest layer {i} into box {id} ({name})"))?;
        prev = Some(id);
        if is_base {
            base_id = id;
            base_name_out = name.clone();
        }
        top_id = id;
        top_name = name;
    }
    Ok(LoadOutcome {
        base_id, base_name: base_name_out,
        top_id, top_name,
        n_layers: n,
        verified: false,   // set by `load` after a policy-required cosign check
    })
}

// ── per-layer tar entry loop ────────────────────────────────────────────────

fn ingest_layer(b: &BoxState, blob: &[u8], media_type: &str) -> Result<()> {
    let reader = decompressor(blob, media_type)?;
    let mut ar = tar::Archive::new(reader);
    ar.set_preserve_permissions(true);
    ar.set_preserve_mtime(true);
    // Phony writer row id: at-rest ingest has no process attribution.
    let writer = 0i64;
    // Tar producers are NOT required to emit a Directory entry for every
    // parent dir before the contents inside it (mtree-style implicit dirs).
    // Without an explicit Dir row, FUSE's lookup of `foo/bar` returns ENOENT
    // even though `foo/bar/baz` lives in this layer — so `ls /foo/bar` would
    // fail. We track which dir rels we've already materialized and create a
    // mode-0o755 placeholder for any ancestor missing one as we walk entries.
    let mut ensured_dirs: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let ensure_ancestors = |b: &BoxState, rel: &str,
                            ensured: &mut std::collections::HashSet<String>| {
        let mut acc = String::new();
        for comp in rel.split('/') {
            if acc.is_empty() { acc.push_str(comp); }
            else { acc.push('/'); acc.push_str(comp); }
            // The leaf itself isn't an ancestor — only stop one shy.
            if acc == rel { break; }
            if ensured.insert(acc.clone()) {
                // Don't clobber a non-Dir row a real entry will set later.
                if !matches!(b.entry(&acc),
                    Some(crate::capture::Entry::Dir { .. })) {
                    b.set_dir(&acc, 0o755, 0);
                }
            }
        }
    };
    for entry in ar.entries().context("read tar entries")? {
        let mut e = entry.context("tar entry")?;
        // Use Entry::path / link_name, NOT header().path() — the Entry-level
        // accessors honor PAX `path=` and GNU `L` long-name extensions, so
        // names past the 100/155-byte USTAR limit aren't silently truncated.
        let raw_path = match e.path() {
            Ok(p) => p.into_owned(),
            Err(_) => continue,
        };
        // Safety: refuse absolute paths or any `..` traversal.
        if raw_path.is_absolute()
            || raw_path.components().any(|c| matches!(c,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)))
        {
            continue;
        }
        let rel_string = raw_path.to_string_lossy().to_string();
        let rel = rel_string.trim_end_matches('/');
        if rel.is_empty() {
            continue;
        }
        let basename = raw_path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // ── AUFS/OCI whiteout convention ──
        //   .wh..wh..opq  → opaque-dir marker (mask the lower at this dir's
        //                   parent). The PARENT of the marker is the dir
        //                   being opacified; we set its sqlar.opaque=1 so
        //                   overlay's resolve()/scan_dir honor it.
        //   .wh.<NAME>    → tombstone for sibling NAME at parent(path).
        if basename == ".wh..wh..opq" {
            let parent_rel = raw_path.parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            // An opaque marker at the layer root (.wh..wh..opq directly at
            // the tar's top) sets parent_rel to "" — the BOX ROOT, which IS
            // a valid opaque target. set_opaque writes a sqlar row with
            // name="" and the overlay's has_opaque_ancestor walks down to it.
            b.set_opaque(&parent_rel, writer);
            continue;
        }
        if let Some(orig) = basename.strip_prefix(".wh.") {
            let parent_rel = raw_path.parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let target = if parent_rel.is_empty() { orig.to_string() }
                         else { format!("{parent_rel}/{orig}") };
            b.set_whiteout(&target, writer);
            continue;
        }
        // Snapshot header fields into locals so the &header borrow ends
        // BEFORE we touch &mut e (pax_extensions / body read).
        let (mode, mtime_ns, uid, gid, ent_type, dev_major, dev_minor) = {
            let h = e.header();
            (
                (h.mode().unwrap_or(0o644)) & 0o7777,
                (h.mtime().unwrap_or(0) as i64).saturating_mul(1_000_000_000),
                h.uid().unwrap_or(0) as u32,
                h.gid().unwrap_or(0) as u32,
                h.entry_type(),
                h.device_major().ok().flatten().unwrap_or(0),
                h.device_minor().ok().flatten().unwrap_or(0),
            )
        };
        // PAX extensions BEFORE the body read consumes them.
        let xattrs = read_pax_xattrs(&mut e);
        // Materialise any missing ancestor dirs before we touch the leaf.
        ensure_ancestors(b, rel, &mut ensured_dirs);
        match ent_type {
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let full_mode = 0o100000 | mode;
                let rowid = b.ensure_file_row(rel, full_mode, writer);
                let bp = crate::depot::blob_path(b.id, rowid);
                if let Some(p) = bp.parent() {
                    std::fs::create_dir_all(p)?;
                }
                let mut out = File::create(&bp)
                    .with_context(|| format!("create blob {}", bp.display()))?;
                let sz = std::io::copy(&mut e, &mut out)
                    .with_context(|| format!("write blob {}", bp.display()))?;
                drop(out);
                let _ = std::fs::set_permissions(&bp,
                    std::fs::Permissions::from_mode(mode));
                b.finalize_file(rel, sz as i64, mtime_ns, writer);
            }
            tar::EntryType::Directory => {
                b.set_dir(rel, mode, writer);
            }
            tar::EntryType::Symlink => {
                let tgt = e.link_name()
                    .ok().flatten()
                    .map(|p| p.into_owned())
                    .unwrap_or_default();
                b.set_symlink(rel, &tgt, writer);
            }
            tar::EntryType::Link => {
                // Hardlink: tar carries a link_name pointing at an earlier
                // entry in this SAME archive. Approximate as a fresh row with
                // the source bytes copied in — the existing FUSE link() does
                // the same approximation (nlink stays 1, but the second name
                // works for the box's processes; matches the Python engine's
                // _link_overlay). Mode comes from the SOURCE: tar Link entries
                // commonly have mode=0 in the header because the inode metadata
                // is supposed to be shared with the target.
                if let Some(src_path) = e.link_name().ok().flatten() {
                    let src_rel = src_path.to_string_lossy()
                        .trim_end_matches('/')
                        .to_string();
                    if let Some((src_rowid, src_mode)) = lookup_file(b, &src_rel) {
                        let src_blob = crate::depot::blob_path(b.id, src_rowid);
                        let new_rowid = b.ensure_file_row(rel, src_mode, writer);
                        let new_blob = crate::depot::blob_path(b.id, new_rowid);
                        if let Some(p) = new_blob.parent() {
                            let _ = std::fs::create_dir_all(p);
                        }
                        std::fs::copy(&src_blob, &new_blob)
                            .with_context(|| format!("hardlink {rel} ← {src_rel}"))?;
                        let sz = std::fs::metadata(&new_blob)?.len() as i64;
                        b.finalize_file(rel, sz, mtime_ns, writer);
                    }
                    // If the source isn't recorded yet (entries out of order),
                    // we skip — uncommon enough that v1 lets it slide. A
                    // second pass would catch it; not worth the complexity here.
                }
            }
            tar::EntryType::Fifo | tar::EntryType::Char | tar::EntryType::Block => {
                let kind = match ent_type {
                    tar::EntryType::Fifo => libc::S_IFIFO,
                    tar::EntryType::Char => libc::S_IFCHR,
                    tar::EntryType::Block => libc::S_IFBLK,
                    _ => unreachable!(),
                };
                let rdev = ((dev_major as u64) << 8) | (dev_minor as u64);
                b.set_special(rel, kind | mode, rdev, writer);
            }
            // tar internal types (XHeader, XGlobalHeader, GNULongName,
            // GNULongLink, GNUSparse, GNULongLink, etc.) are consumed by the
            // tar crate itself — Entry::path()/link_name() already honor them.
            _ => continue,
        }
        if uid != 0 || gid != 0 {
            b.set_owner(rel, uid, gid);
        }
        for (key, val) in xattrs {
            b.set_xattr(rel, &key, &val);
        }
    }
    Ok(())
}

/// PAX `SCHILY.xattr.*` entries — important for distroless / SELinux-labeled
/// images and easy to silently drop. Returns (key without prefix, raw bytes).
fn read_pax_xattrs<R: Read>(e: &mut tar::Entry<'_, R>) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let Ok(Some(exts)) = e.pax_extensions() else { return out; };
    for ext in exts.flatten() {
        let Ok(key) = ext.key() else { continue; };
        if let Some(name) = key.strip_prefix("SCHILY.xattr.") {
            out.push((name.to_string(), ext.value_bytes().to_vec()));
        }
    }
    out
}

/// (rowid, full mode) of a regular file already ingested into `b`. Used by
/// the hardlink path — sources are always earlier entries in the same tar,
/// and tar Link entries don't carry mode so we copy it from the source.
fn lookup_file(b: &BoxState, rel: &str) -> Option<(i64, u32)> {
    match b.entry(rel) {
        Some(crate::capture::Entry::File { rowid, mode }) => Some((rowid, mode)),
        _ => None,
    }
}

// ── decompression ───────────────────────────────────────────────────────────

fn decompressor<'a>(blob: &'a [u8], media_type: &str)
    -> Result<Box<dyn Read + 'a>>
{
    // Dispatch on media type first, with a magic-byte safety net for the
    // small but real case of a manifest mis-labelling the layer (some early
    // BuildKit releases shipped zstd bytes under the gzip media type).
    let actual = sniff(blob).unwrap_or_else(|| classify(media_type));
    match actual {
        Comp::Gzip => Ok(Box::new(flate2::read::GzDecoder::new(blob))),
        Comp::Zstd => {
            let d = ruzstd::StreamingDecoder::new(blob)
                .map_err(|e| anyhow!("zstd init: {e}"))?;
            Ok(Box::new(d))
        }
        // xz has no streaming Read adapter in lzma-rs, so decode the whole blob
        // into memory and hand back a cursor. ADD sources are local files we
        // already read fully, so this allocates no more than the layer itself.
        Comp::Xz => {
            let mut out = Vec::new();
            let mut input = blob;
            lzma_rs::xz_decompress(&mut input, &mut out)
                .map_err(|e| anyhow!("xz decode: {e}"))?;
            Ok(Box::new(std::io::Cursor::new(out)))
        }
        // bzip2-rs DecoderReader streams, borrowing the blob.
        Comp::Bzip2 => Ok(Box::new(bzip2_rs::DecoderReader::new(blob))),
        Comp::None => Ok(Box::new(blob)),
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum Comp { Gzip, Zstd, Xz, Bzip2, None }

fn classify(media_type: &str) -> Comp {
    if media_type.ends_with("+gzip") || media_type.ends_with(".gzip") {
        Comp::Gzip
    } else if media_type.ends_with("+zstd") || media_type.ends_with("+zstd+chunked") {
        // zstd:chunked is a containers-storage extension; the blob itself is
        // still a valid zstd stream, so plain zstd decoding is correct (just
        // doesn't take advantage of the chunked-TOC fast path — v1 is fine).
        Comp::Zstd
    } else {
        Comp::None
    }
}

fn sniff(blob: &[u8]) -> Option<Comp> {
    if blob.len() >= 6 && blob[..6] == [0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00] {
        return Some(Comp::Xz);
    }
    if blob.len() >= 3 && &blob[..3] == b"BZh" {
        return Some(Comp::Bzip2);
    }
    if blob.len() >= 4 {
        if blob[0] == 0x1f && blob[1] == 0x8b {
            return Some(Comp::Gzip);
        }
        if &blob[..4] == [0x28, 0xb5, 0x2f, 0xfd] {
            return Some(Comp::Zstd);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::paths_lcp;

    fn v(xs: &[&str]) -> Vec<String> { xs.iter().map(|s| s.to_string()).collect() }

    #[test]
    fn lcp_single_path_is_itself() {
        assert_eq!(paths_lcp(&v(&["app/bin/tool"])), "app/bin/tool");
    }

    #[test]
    fn lcp_shared_dir() {
        assert_eq!(paths_lcp(&v(&["app/a", "app/b", "app/c"])), "app");
    }

    #[test]
    fn lcp_nested_common() {
        assert_eq!(paths_lcp(&v(&["app/src/x", "app/src/y"])), "app/src");
    }

    #[test]
    fn lcp_divergent_is_empty() {
        assert_eq!(paths_lcp(&v(&["etc/x", "usr/y"])), "");
    }

    #[test]
    fn lcp_no_partial_component_match() {
        // "app" and "apple" share no path component even though they share a
        // string prefix — LCP is component-wise, so this degrades to "".
        assert_eq!(paths_lcp(&v(&["app/x", "apple/y"])), "");
    }

    #[test]
    fn lcp_empty_input() {
        assert_eq!(paths_lcp(&[]), "");
    }
}
