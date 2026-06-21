// Dockerfile / Containerfile parser for `sarun oci build` / `sarun oci run`.
//
// First-party (no new dependency), but the *tricky* parsing details and the
// test corpus are ported from the reference implementations rather than
// invented — so behavior matches what real Dockerfiles rely on:
//
//   * moby/buildkit  frontend/dockerfile/parser/{directives,parser}.go
//                    frontend/dockerfile/instructions/parse.go         (Apache-2.0)
//   * moby/buildkit  frontend/dockerfile/instructions/parse_test.go    (Apache-2.0)
//   * openshift/imagebuilder dockerfile/parser  (what podman/buildah use, Apache-2.0)
//
// Each non-obvious rule below cites the reference behavior it mirrors. Where
// buildkit's newest parser diverges from the long-documented Docker behavior
// (e.g. the legacy `ENV key value` form), the divergence is called out at the
// site and we keep the documented, user-facing semantics.
//
// Scope: the instruction subset that matters for "build software using a
// toolchain distributed as a container image." Instructions that are KNOWN to
// Docker but irrelevant to a from-source build of a vendor blob (MAINTAINER,
// HEALTHCHECK, ONBUILD, STOPSIGNAL) are carried as `Unsupported` (surfaced,
// never silently dropped — the no-silent-downgrade rule that governs the rest
// of sarun). A verb that is not a Docker instruction at all is a hard parse
// error ("unknown instruction"), matching moby.

use std::collections::HashMap;

/// A RUN / CMD / ENTRYPOINT body: either a raw shell string (shell form) or an
/// explicit argv vector (JSON "exec form", `["a","b"]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmdline {
    /// `RUN make install` — run through the image's shell (`/bin/sh -c`).
    Shell(String),
    /// `RUN ["make", "install"]` — exec'd directly, no shell.
    Exec(Vec<String>),
}

/// One parsed Dockerfile instruction. Strings are RAW (unexpanded) — the
/// builder runs `expand()` over them with the accumulated build vars.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instruction {
    From {
        image: String,
        platform: Option<String>,
        stage_as: Option<String>,
    },
    Run(Cmdline),
    Copy {
        sources: Vec<String>,
        dest: String,
        from: Option<String>,
        chown: Option<String>,
        chmod: Option<String>,
    },
    Add {
        sources: Vec<String>,
        dest: String,
        chown: Option<String>,
        chmod: Option<String>,
    },
    Env(Vec<(String, String)>),
    Arg {
        name: String,
        default: Option<String>,
    },
    Workdir(String),
    User(String),
    Entrypoint(Cmdline),
    Cmd(Cmdline),
    Label(Vec<(String, String)>),
    Expose(String),
    Volume(Vec<String>),
    Shell(Vec<String>),
    /// `STOPSIGNAL signal` — carried into the image config's StopSignal.
    Stopsignal(String),
    /// `ONBUILD <instruction>` — the trailing instruction text, stored
    /// verbatim into the image config's OnBuild list (Docker semantics: the
    /// trigger fires when THIS image is later used as a base, which is out of
    /// our build's scope, but the image must still carry it faithfully).
    Onbuild(String),
    /// `HEALTHCHECK NONE` | `HEALTHCHECK [opts] CMD …` — carried into the
    /// image config's Healthcheck.
    Healthcheck(HealthcheckSpec),
    /// A verb Docker knows but we don't act on (only MAINTAINER now — it maps
    /// to the deprecated `author` field). Carried, not dropped, so the builder
    /// warns.
    Unsupported { verb: String, rest: String },
}

/// Parsed HEALTHCHECK. `none` (from `HEALTHCHECK NONE`) disables an inherited
/// check; otherwise `test` is the probe command and the options are raw
/// duration strings (`30s`, `5m`) / retry count, converted to the image
/// config's nanosecond ints by the builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthcheckSpec {
    pub none: bool,
    pub test: Option<Cmdline>,
    pub interval: Option<String>,
    pub timeout: Option<String>,
    pub start_period: Option<String>,
    pub start_interval: Option<String>,
    pub retries: Option<u32>,
}

/// A parsed Dockerfile: the ordered instructions plus the 1-based line each
/// came from (the logical line's FIRST physical line) for diagnostics.
#[derive(Debug, Clone)]
pub struct Dockerfile {
    pub instructions: Vec<(usize, Instruction)>,
    /// The escape character in force (`\` default, `` ` `` if a `# escape=`
    /// directive selected it). Exposed for callers that re-tokenize.
    pub escape: char,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub msg: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Dockerfile line {}: {}", self.line, self.msg)
    }
}
impl std::error::Error for ParseError {}

/// The Docker instruction verbs we recognize. Anything outside this set is an
/// "unknown instruction" error (moby instructions/parse.go: `errors.Errorf(
/// "unknown instruction: %s", ...)`).
const KNOWN_VERBS: &[&str] = &[
    "FROM", "RUN", "CMD", "ENTRYPOINT", "COPY", "ADD", "ENV", "ARG", "WORKDIR",
    "USER", "LABEL", "EXPOSE", "VOLUME", "SHELL", "MAINTAINER", "HEALTHCHECK",
    "ONBUILD", "STOPSIGNAL",
];

impl Dockerfile {
    pub fn parse(text: &str) -> Result<Dockerfile, ParseError> {
        // moby parser.go discards a leading UTF-8 BOM before anything else.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let escape = parse_directives(text)?;
        let logical = join_logical_lines(text, escape);
        let mut instructions = Vec::new();
        for (line_no, raw) in logical {
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue; // join_logical_lines already drops these; belt+braces
            }
            let (verb, rest) = split_verb(trimmed);
            let inst = parse_instruction(&verb, rest, line_no)?;
            instructions.push((line_no, inst));
        }
        Ok(Dockerfile { instructions, escape })
    }
}

// ── parser directives (escape / syntax / check) ──────────────────────────────
// moby/buildkit frontend/dockerfile/parser/directives.go:
//   directiveRegexp = `^([a-zA-Z][a-zA-Z0-9]*)\s*=\s*(.+?)\s*$` applied to the
//   text AFTER the leading `#`. Only `syntax`, `escape`, `check` are valid;
//   names are lowercased; each may appear at most once; processing stops
//   ("done = true") at the FIRST line that is not a matching directive comment.
//   A leading `#!` shebang line is skipped. The escape token must be `\` or
//   `` ` `` — anything else is an error.

fn parse_directives(text: &str) -> Result<char, ParseError> {
    let mut escape = '\\';
    let mut seen: Vec<String> = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line_no = idx + 1;
        // A shebang at the very top is discarded, scanning continues.
        if line.starts_with("#!") {
            continue;
        }
        let t = line.trim_start();
        // First non-comment line ends the directive zone (done = true).
        let Some(body) = t.strip_prefix('#') else { break };
        let Some((key, val)) = directive_kv(body) else {
            // A comment that isn't `key = value` shape also ends the zone.
            break;
        };
        let key = key.to_ascii_lowercase();
        if !matches!(key.as_str(), "syntax" | "escape" | "check") {
            break;
        }
        if seen.contains(&key) {
            return Err(ParseError {
                line: line_no,
                msg: format!("only one {key} parser directive can be used"),
            });
        }
        seen.push(key.clone());
        if key == "escape" {
            escape = match val {
                "\\" => '\\',
                "`" => '`',
                other => {
                    return Err(ParseError {
                        line: line_no,
                        msg: format!(
                            "invalid escape token '{other}': must be \\ or `"
                        ),
                    });
                }
            };
        }
        // syntax / check are consumed (uniqueness enforced) but not acted on.
    }
    Ok(escape)
}

/// Match the directive regex `^([a-zA-Z][a-zA-Z0-9]*)\s*=\s*(.+?)\s*$` against
/// a comment body (the text after `#`). Returns (key, value) on a match.
fn directive_kv(body: &str) -> Option<(&str, &str)> {
    let body = body.trim();
    let eq = body.find('=')?;
    let key = body[..eq].trim_end();
    let val = body[eq + 1..].trim_start();
    if key.is_empty() || val.is_empty() {
        return None;
    }
    let mut chars = key.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some((key, val))
}

// ── logical-line assembly (line continuation) ────────────────────────────────
// moby/buildkit frontend/dockerfile/parser/parser.go:
//   lineContinuationRegex = `([^\\])\\[ \t]*$|^\\[ \t]*$` (with `\` swapped for
//   the active escape char). A line whose last non-whitespace char is the
//   escape char continues onto the next — UNLESS that escape is itself escaped
//   (`\\` at EOL is a literal backslash, NOT a continuation). Trailing
//   whitespace after the escape is dropped; the char BEFORE the escape and all
//   LEADING whitespace are preserved (so `RUN a \` + `  b` → `RUN a   b`).
//   Comment lines and blank ("empty continuation") lines encountered WHILE
//   continuing are skipped. An unterminated continuation at EOF is taken as-is.

fn join_logical_lines(text: &str, escape: char) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut start_line = 0usize;
    let mut continuing = false;
    for (idx, line) in text.lines().enumerate() {
        let line_no = idx + 1;
        let blank = line.trim().is_empty();
        let comment = line.trim_start().starts_with('#');
        if continuing {
            // Mid-continuation comment / blank lines are dropped (no append).
            if comment || blank {
                continue;
            }
        } else {
            if blank || comment {
                continue;
            }
            start_line = line_no;
        }
        match continuation_content(line, escape) {
            Some(pre) => {
                buf.push_str(pre);
                continuing = true;
            }
            None => {
                buf.push_str(line);
                out.push((start_line, std::mem::take(&mut buf)));
                continuing = false;
            }
        }
    }
    if continuing && !buf.is_empty() {
        out.push((start_line, buf));
    }
    out
}

/// If `line` ends in an (unescaped) continuation, return the content with the
/// trailing escape char and trailing horizontal whitespace removed; else None.
fn continuation_content(line: &str, escape: char) -> Option<&str> {
    let no_trail = line.trim_end_matches([' ', '\t']);
    if !no_trail.ends_with(escape) {
        return None;
    }
    let pre = &no_trail[..no_trail.len() - escape.len_utf8()];
    // `\\` (escape preceded by escape) is a literal — not a continuation. A
    // solo escape line (`pre` empty) IS a continuation (`^\\[ \t]*$`).
    if !pre.is_empty() && pre.ends_with(escape) {
        return None;
    }
    Some(pre)
}

/// Split "VERB rest..." → (UPPERCASE verb, rest). Verb match is case-insensitive
/// (moby lowercases/compares case-insensitively); rest keeps its case.
fn split_verb(line: &str) -> (String, &str) {
    let line = line.trim_start();
    match line.find(char::is_whitespace) {
        Some(i) => (line[..i].to_ascii_uppercase(), line[i + 1..].trim_start()),
        None => (line.to_ascii_uppercase(), ""),
    }
}

fn parse_instruction(verb: &str, rest: &str, line: usize)
    -> Result<Instruction, ParseError>
{
    let err = |m: String| ParseError { line, msg: m };
    Ok(match verb {
        "FROM" => parse_from(rest).map_err(err)?,
        "RUN" => Instruction::Run(parse_run(rest)),
        "CMD" => Instruction::Cmd(parse_cmdline(rest)),
        "ENTRYPOINT" => Instruction::Entrypoint(parse_cmdline(rest)),
        "COPY" => parse_copy_add(rest, false).map_err(err)?,
        "ADD" => parse_copy_add(rest, true).map_err(err)?,
        "ENV" => Instruction::Env(parse_kv(rest, "ENV").map_err(err)?),
        "LABEL" => Instruction::Label(parse_kv(rest, "LABEL").map_err(err)?),
        "ARG" => parse_arg(rest).map_err(err)?,
        "WORKDIR" => {
            let w = rest.trim();
            if w.is_empty() {
                return Err(err("WORKDIR requires exactly one argument".into()));
            }
            Instruction::Workdir(w.to_string())
        }
        "USER" => {
            let u = rest.trim();
            if u.is_empty() {
                return Err(err("USER requires exactly one argument".into()));
            }
            Instruction::User(u.to_string())
        }
        "EXPOSE" => {
            let e = rest.trim();
            if e.is_empty() {
                return Err(err("EXPOSE requires at least one argument".into()));
            }
            Instruction::Expose(e.to_string())
        }
        "VOLUME" => {
            let v = parse_string_list(rest);
            if v.is_empty() {
                return Err(err("VOLUME requires at least one argument".into()));
            }
            Instruction::Volume(v)
        }
        "SHELL" => {
            let v = parse_json_array(rest)
                .ok_or_else(|| err("SHELL requires a JSON array".into()))?;
            Instruction::Shell(v)
        }
        "STOPSIGNAL" => {
            let s = rest.trim();
            if s.is_empty() {
                return Err(err("STOPSIGNAL requires a signal".into()));
            }
            Instruction::Stopsignal(s.to_string())
        }
        "ONBUILD" => parse_onbuild(rest, line).map_err(err)?,
        "HEALTHCHECK" => Instruction::Healthcheck(parse_healthcheck(rest).map_err(err)?),
        // Known to Docker, irrelevant to a from-source build: carry, don't drop.
        "MAINTAINER" => {
            Instruction::Unsupported { verb: verb.to_string(), rest: rest.to_string() }
        }
        other => {
            // moby: `unknown instruction: FOO`.
            return Err(err(format!("unknown instruction: {other}")));
        }
    })
}

fn parse_from(rest: &str) -> Result<Instruction, String> {
    let mut platform = None;
    let mut toks: Vec<&str> = Vec::new();
    for t in rest.split_whitespace() {
        if let Some(p) = t.strip_prefix("--platform=") {
            platform = Some(p.to_string());
        } else {
            toks.push(t);
        }
    }
    if toks.is_empty() {
        return Err("FROM requires an image reference".into());
    }
    let image = toks[0].to_string();
    // `FROM image AS name` (AS is case-insensitive in moby).
    let stage_as = match toks.get(1).map(|s| s.to_ascii_uppercase()) {
        Some(ref kw) if kw == "AS" => toks.get(2).map(|s| s.to_string()),
        _ => None,
    };
    Ok(Instruction::From { image, platform, stage_as })
}

/// RUN: strip leading BuildKit `--flag[=val]` tokens (e.g. `--mount`,
/// `--network`, `--security`) that precede the command, then parse the
/// remainder as shell/exec form. We don't honor mount/network/security flags
/// (out of scope — see module header), but a cache `--mount` is
/// correctness-preserving to drop (the RUN still executes, just without the
/// cache), so dropping it is safe rather than a silent semantic change.
fn parse_run(rest: &str) -> Cmdline {
    let mut s = rest.trim_start();
    while let Some(after) = s.strip_prefix("--") {
        // A flag is `--word...` up to whitespace; the first token NOT starting
        // with `--` begins the command (moby parses flags before the command).
        let end = after.find(char::is_whitespace).unwrap_or(after.len());
        let _flag = &after[..end];
        s = after[end..].trim_start();
        if s.is_empty() {
            break;
        }
    }
    parse_cmdline(s)
}

/// RUN/CMD/ENTRYPOINT: a valid JSON string array → exec form; otherwise shell
/// form verbatim. moby sets `attributes["json"]` when the args parse as JSON;
/// PrependShell is the negation of that.
fn parse_cmdline(rest: &str) -> Cmdline {
    if let Some(v) = parse_json_array(rest) {
        Cmdline::Exec(v)
    } else {
        Cmdline::Shell(rest.trim().to_string())
    }
}

fn parse_copy_add(rest: &str, is_add: bool) -> Result<Instruction, String> {
    let name = if is_add { "ADD" } else { "COPY" };
    let mut from = None;
    let mut chown = None;
    let mut chmod = None;
    // Flags precede the operands. Operands may be a JSON array
    // (`COPY ["src","dest"]`) or whitespace-separated.
    let mut operand_str = rest.trim();
    loop {
        let t = operand_str.trim_start();
        let Some(flagrest) = t.strip_prefix("--") else {
            operand_str = t;
            break;
        };
        let (flag, tail) = match flagrest.find(char::is_whitespace) {
            Some(i) => (&flagrest[..i], &flagrest[i + 1..]),
            None => (flagrest, ""),
        };
        if let Some(v) = flag.strip_prefix("from=") {
            from = Some(v.to_string());
        } else if let Some(v) = flag.strip_prefix("chown=") {
            chown = Some(v.to_string());
        } else if let Some(v) = flag.strip_prefix("chmod=") {
            chmod = Some(v.to_string());
        } else if flag == "link" || flag.starts_with("link=")
            || flag == "parents" || flag.starts_with("exclude=")
            || flag == "keep-git-dir" || flag.starts_with("checksum=")
        {
            // Recognized buildkit COPY/ADD flags we don't need to act on for
            // our model — accepted (not an error) and ignored.
        } else {
            return Err(format!("unknown {name} flag --{flag}"));
        }
        operand_str = tail;
    }
    let mut operands = if let Some(v) = parse_json_array(operand_str) {
        v
    } else {
        operand_str.split_whitespace().map(|s| s.to_string()).collect()
    };
    // moby: COPY/ADD need >= 2 args (>=1 source + dest).
    if operands.len() < 2 {
        return Err(format!("{name} requires at least two arguments"));
    }
    let dest = operands.pop().unwrap();
    let sources = operands;
    Ok(if is_add {
        Instruction::Add { sources, dest, chown, chmod }
    } else {
        Instruction::Copy { sources, dest, from, chown, chmod }
    })
}

/// ENV/LABEL key/value parsing. Two legal shapes:
///   ENV KEY value with spaces        (legacy single-pair, value = rest-of-line)
///   ENV KEY=val KEY2="a b" KEY3=c     (modern multi-pair, shell-ish quoting)
///
/// Divergence note: buildkit's NEWEST parser rejects `ENV a b c` ("too many
/// arguments") because it word-splits first. We keep the long-DOCUMENTED Docker
/// behavior ("the entire string after the first space is the value, including
/// whitespace") for the legacy form, since that's what existing build files in
/// the wild rely on. Modern `KEY=VALUE` form is identical to buildkit.
fn parse_kv(rest: &str, verb: &str) -> Result<Vec<(String, String)>, String> {
    let rest = rest.trim();
    if rest.is_empty() {
        return Err(format!("{verb} requires at least one argument"));
    }
    let first_tok_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let first_tok = &rest[..first_tok_end];
    if !first_tok.contains('=') {
        // Legacy form: `KEY <rest of line>`.
        let key = first_tok.to_string();
        let val = rest[first_tok_end..].trim().to_string();
        if val.is_empty() {
            return Err(format!("{verb} requires name=value (or `name value`)"));
        }
        return Ok(vec![(key, unquote(&val))]);
    }
    // Modern form: tokenize respecting quotes; each token is KEY=VALUE.
    let toks = tokenize_quoted(rest);
    let mut out = Vec::new();
    for t in toks {
        let (k, v) = t.split_once('=')
            .ok_or_else(|| format!("{verb} token '{t}' is not KEY=VALUE"))?;
        if k.is_empty() {
            // moby: `ENV requires name=value` on a blank name (`ENV =arg`).
            return Err(format!("{verb} requires name=value"));
        }
        out.push((k.to_string(), unquote(v)));
    }
    Ok(out)
}

fn parse_arg(rest: &str) -> Result<Instruction, String> {
    let rest = rest.trim();
    if rest.is_empty() {
        return Err("ARG requires exactly one argument".into());
    }
    match rest.split_once('=') {
        Some((n, d)) => {
            let name = n.trim().to_string();
            if name.is_empty() {
                return Err("ARG requires a name".into());
            }
            Ok(Instruction::Arg { name, default: Some(unquote(d.trim())) })
        }
        None => Ok(Instruction::Arg { name: rest.to_string(), default: None }),
    }
}

/// `ONBUILD <instruction>` — the trigger instruction stored verbatim. Docker
/// forbids chaining (`ONBUILD ONBUILD`) and `ONBUILD FROM`/`ONBUILD MAINTAINER`
/// (instructions/parse.go). We validate the trailing verb but keep the rest of
/// the line as-is, since that's what the image config's OnBuild list stores.
fn parse_onbuild(rest: &str, _line: usize) -> Result<Instruction, String> {
    let rest = rest.trim();
    if rest.is_empty() {
        return Err("ONBUILD requires an instruction".into());
    }
    let (verb, _) = split_verb(rest);
    match verb.as_str() {
        "ONBUILD" => return Err("Chaining ONBUILD via `ONBUILD ONBUILD` isn't allowed".into()),
        "FROM" | "MAINTAINER" => return Err(format!("{verb} isn't allowed as an ONBUILD trigger")),
        _ => {}
    }
    Ok(Instruction::Onbuild(rest.to_string()))
}

/// `HEALTHCHECK NONE` | `HEALTHCHECK [--interval=D] [--timeout=D]
/// [--start-period=D] [--start-interval=D] [--retries=N] CMD command…`.
/// Mirrors moby instructions/parse.go: options precede a required `CMD`
/// keyword; the command after it is shell or exec form.
fn parse_healthcheck(rest: &str) -> Result<HealthcheckSpec, String> {
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        return Err("HEALTHCHECK requires an argument".into());
    }
    if trimmed.eq_ignore_ascii_case("none") {
        return Ok(HealthcheckSpec {
            none: true, test: None, interval: None, timeout: None,
            start_period: None, start_interval: None, retries: None,
        });
    }
    let mut spec = HealthcheckSpec {
        none: false, test: None, interval: None, timeout: None,
        start_period: None, start_interval: None, retries: None,
    };
    // Strip leading `--opt=val` flags up to the `CMD` keyword.
    let mut s = trimmed;
    loop {
        let t = s.trim_start();
        let Some(flagrest) = t.strip_prefix("--") else { s = t; break; };
        let (flag, tail) = match flagrest.find(char::is_whitespace) {
            Some(i) => (&flagrest[..i], &flagrest[i + 1..]),
            None => (flagrest, ""),
        };
        let (k, v) = flag.split_once('=')
            .ok_or_else(|| format!("HEALTHCHECK flag --{flag} needs a value"))?;
        match k {
            "interval" => spec.interval = Some(v.to_string()),
            "timeout" => spec.timeout = Some(v.to_string()),
            "start-period" => spec.start_period = Some(v.to_string()),
            "start-interval" => spec.start_interval = Some(v.to_string()),
            "retries" => spec.retries = Some(v.parse()
                .map_err(|_| format!("HEALTHCHECK --retries wants an integer, got '{v}'"))?),
            other => return Err(format!("unknown HEALTHCHECK flag --{other}")),
        }
        s = tail;
    }
    let (kw, cmd) = split_verb(s.trim_start());
    if kw != "CMD" {
        return Err("HEALTHCHECK needs `CMD` (or `NONE`)".into());
    }
    if cmd.trim().is_empty() {
        return Err("HEALTHCHECK CMD requires a command".into());
    }
    spec.test = Some(parse_cmdline(cmd));
    Ok(spec)
}

/// VOLUME accepts a JSON array or whitespace list.
fn parse_string_list(rest: &str) -> Vec<String> {
    if let Some(v) = parse_json_array(rest) {
        v
    } else {
        rest.split_whitespace().map(|s| s.to_string()).collect()
    }
}

/// Parse a JSON string array `["a", "b"]`. Returns None if the text isn't a
/// well-formed array of strings (so the caller falls back to shell/whitespace
/// form). serde_json handles escapes correctly — matching moby's `parseJSON`.
fn parse_json_array(s: &str) -> Option<Vec<String>> {
    let s = s.trim();
    if !s.starts_with('[') {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let arr = v.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for e in arr {
        out.push(e.as_str()?.to_string());
    }
    Some(out)
}

/// Strip a single layer of matching surrounding quotes from a value.
fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 {
        let b = s.as_bytes();
        if (b[0] == b'"' && b[s.len() - 1] == b'"')
            || (b[0] == b'\'' && b[s.len() - 1] == b'\'')
        {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// Whitespace tokenizer that keeps quoted spans together (single or double
/// quotes, with `\`-escapes inside). Quotes are stripped per-token. Mirrors the
/// shell-ish splitting moby applies to ENV/LABEL `KEY="a b"`; not a full shell
/// lexer (we don't need variable expansion here — `expand()` does that later).
fn tokenize_quoted(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut have = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' {
                    if let Some(&n) = chars.peek() {
                        cur.push(n);
                        chars.next();
                    }
                } else {
                    cur.push(c);
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                    have = true;
                } else if c.is_whitespace() {
                    if have {
                        out.push(std::mem::take(&mut cur));
                        have = false;
                    }
                } else {
                    cur.push(c);
                    have = true;
                }
            }
        }
    }
    if have {
        out.push(cur);
    }
    out
}

/// Expand `$VAR` and `${VAR}` references in `s` against `vars`. Unknown vars
/// expand to empty (Docker semantics). `$$` is a literal `$`; `\$` escapes the
/// dollar. Supports `${VAR:-default}` and `${VAR:+alt}`. Applied by the BUILDER
/// (not the parser) so command-line `--build-arg` values participate.
pub fn expand(s: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '\\' && i + 1 < bytes.len() && bytes[i + 1] == b'$' {
            out.push('$');
            i += 2;
            continue;
        }
        if c == '$' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'$' {
                out.push('$');
                i += 2;
                continue;
            }
            if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                if let Some(end) = s[i + 2..].find('}') {
                    let name = &s[i + 2..i + 2 + end];
                    let (key, modref) = split_modifier(name);
                    out.push_str(&apply_modifier(vars.get(key), modref));
                    i = i + 2 + end + 1;
                    continue;
                }
            }
            // Bare `$NAME` — name is [A-Za-z_][A-Za-z0-9_]*.
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() {
                let ch = bytes[j];
                let ok = ch == b'_'
                    || ch.is_ascii_alphabetic()
                    || (j > start && ch.is_ascii_digit());
                if !ok {
                    break;
                }
                j += 1;
            }
            if j > start {
                if let Some(v) = vars.get(&s[start..j]) {
                    out.push_str(v);
                }
                i = j;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

fn split_modifier(name: &str) -> (&str, Option<(char, &str)>) {
    if let Some(idx) = name.find(":-") {
        return (&name[..idx], Some(('-', &name[idx + 2..])));
    }
    if let Some(idx) = name.find(":+") {
        return (&name[..idx], Some(('+', &name[idx + 2..])));
    }
    (name, None)
}

fn apply_modifier(val: Option<&String>, modref: Option<(char, &str)>) -> String {
    match modref {
        Some(('-', dflt)) => match val {
            Some(v) if !v.is_empty() => v.clone(),
            _ => dflt.to_string(),
        },
        Some(('+', alt)) => match val {
            Some(v) if !v.is_empty() => alt.to_string(),
            _ => String::new(),
        },
        _ => val.cloned().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    // Test cases ported from moby/buildkit
    // frontend/dockerfile/instructions/parse_test.go and the documented
    // edge cases in parser/{directives,parser}.go (both Apache-2.0). Each test
    // notes the rule it pins.
    use super::*;
    use std::collections::HashMap;

    fn parse(s: &str) -> Vec<Instruction> {
        Dockerfile::parse(s).unwrap().instructions
            .into_iter().map(|(_, i)| i).collect()
    }
    fn parse_err(s: &str) -> ParseError {
        Dockerfile::parse(s).unwrap_err()
    }

    #[test]
    fn from_basic_and_as() {
        assert_eq!(parse("FROM alpine:3.20"), vec![Instruction::From {
            image: "alpine:3.20".into(), platform: None, stage_as: None }]);
        assert_eq!(parse("FROM --platform=linux/amd64 golang:1.22 AS build"),
            vec![Instruction::From {
                image: "golang:1.22".into(),
                platform: Some("linux/amd64".into()),
                stage_as: Some("build".into()) }]);
        // `as` is case-insensitive.
        assert_eq!(parse("FROM x as y"), vec![Instruction::From {
            image: "x".into(), platform: None, stage_as: Some("y".into()) }]);
    }

    #[test]
    fn run_shell_and_exec() {
        assert_eq!(parse("RUN make install"),
            vec![Instruction::Run(Cmdline::Shell("make install".into()))]);
        assert_eq!(parse(r#"RUN ["make", "install"]"#),
            vec![Instruction::Run(Cmdline::Exec(
                vec!["make".into(), "install".into()]))]);
    }

    #[test]
    fn run_strips_leading_mount_flag() {
        // moby parses RUN flags before the command; `--mount` precedes it.
        assert_eq!(
            parse("RUN --mount=type=cache,target=/root/.cache make"),
            vec![Instruction::Run(Cmdline::Shell("make".into()))]);
        // A `--foo` that's part of the command (not leading) is untouched.
        assert_eq!(parse("RUN echo --foo"),
            vec![Instruction::Run(Cmdline::Shell("echo --foo".into()))]);
    }

    #[test]
    fn line_continuation_joins_preserving_whitespace() {
        // moby preserves leading ws on continued lines: `a \` + `  b` → `a   b`
        // (the trailing space before `\` + the 2 leading spaces).
        let i = parse("RUN apt-get update \\\n  && apt-get install -y gcc");
        assert_eq!(i, vec![Instruction::Run(Cmdline::Shell(
            "apt-get update   && apt-get install -y gcc".into()))]);
    }

    #[test]
    fn escaped_escape_does_not_continue() {
        // `foo\\` (two backslashes) at EOL is a literal `\`, NOT a continuation
        // (lineContinuationRegex requires `[^\\]` before the final `\`).
        let i = parse("RUN echo foo\\\\\nRUN echo bar");
        assert_eq!(i.len(), 2);
        assert_eq!(i[0], Instruction::Run(Cmdline::Shell("echo foo\\\\".into())));
    }

    #[test]
    fn solo_escape_line_continues() {
        // `^\\[ \t]*$` — a line that is only the escape char continues. Escape
        // continuation inserts NOTHING between joined lines (the classic Docker
        // gotcha: `echo a\` + `echo b` → `echo aecho b`, not `echo a echo b`).
        let i = parse("RUN echo a\\\n\\\necho b");
        assert_eq!(i, vec![Instruction::Run(Cmdline::Shell(
            "echo aecho b".into()))]);
    }

    #[test]
    fn continuation_skips_inner_comment_and_blank() {
        // Comment + empty lines inside a continuation are dropped.
        let i = parse("RUN a \\\n# a comment\n\\\n  b");
        assert_eq!(i, vec![Instruction::Run(Cmdline::Shell("a   b".into()))]);
    }

    #[test]
    fn comments_and_blanks_between_instructions_ignored() {
        let i = parse("# header\n\nFROM x\n   # indented comment\nRUN y");
        assert_eq!(i.len(), 2);
    }

    #[test]
    fn escape_directive_backtick() {
        // `# escape=\`` selects backtick as the continuation char.
        let src = "# escape=`\nRUN a `\n  b";
        assert_eq!(parse(src), vec![Instruction::Run(Cmdline::Shell(
            "a   b".into()))]);
    }

    #[test]
    fn escape_directive_only_honored_as_leading_directive() {
        // A non-directive comment BEFORE `# escape=` ends the directive zone,
        // so the escape stays `\` and the backtick line does NOT continue.
        let src = "# hi\n# escape=`\nFROM x";
        let df = Dockerfile::parse(src).unwrap();
        assert_eq!(df.escape, '\\');
    }

    #[test]
    fn duplicate_escape_directive_errors() {
        let e = parse_err("# escape=\\\n# escape=`\nFROM x");
        assert!(e.msg.contains("only one escape"), "{}", e.msg);
    }

    #[test]
    fn invalid_escape_value_errors() {
        let e = parse_err("# escape=x\nFROM y");
        assert!(e.msg.contains("invalid escape"), "{}", e.msg);
    }

    #[test]
    fn shebang_skipped_then_directive() {
        let df = Dockerfile::parse("#!/usr/bin/env foo\n# escape=`\nFROM x")
            .unwrap();
        assert_eq!(df.escape, '`');
    }

    #[test]
    fn env_modern_and_legacy() {
        assert_eq!(parse("ENV A=1 B=\"two words\" C=3"),
            vec![Instruction::Env(vec![
                ("A".into(), "1".into()),
                ("B".into(), "two words".into()),
                ("C".into(), "3".into())])]);
        // Legacy `ENV KEY value-with-spaces` → single var, value = rest-of-line.
        assert_eq!(parse("ENV NAME some value"),
            vec![Instruction::Env(vec![("NAME".into(), "some value".into())])]);
        assert_eq!(parse("ENV PATH /usr/local/bin:/usr/bin"),
            vec![Instruction::Env(vec![
                ("PATH".into(), "/usr/local/bin:/usr/bin".into())])]);
    }

    #[test]
    fn env_errors() {
        // moby: `ENV` (no args) → requires at least one argument.
        assert!(parse_err("ENV").msg.contains("at least one"));
        // moby: `ENV =arg2` (blank name) → requires name=value.
        assert!(parse_err("ENV =arg2").msg.contains("name=value"));
    }

    #[test]
    fn arg_with_and_without_default() {
        assert_eq!(parse("ARG VERSION=1.2.3"), vec![Instruction::Arg {
            name: "VERSION".into(), default: Some("1.2.3".into()) }]);
        assert_eq!(parse("ARG TOKEN"),
            vec![Instruction::Arg { name: "TOKEN".into(), default: None }]);
    }

    #[test]
    fn copy_flags_and_operands() {
        let i = parse("COPY --from=build --chown=0:0 /src/a /src/b /dst/");
        assert_eq!(i, vec![Instruction::Copy {
            sources: vec!["/src/a".into(), "/src/b".into()],
            dest: "/dst/".into(),
            from: Some("build".into()),
            chown: Some("0:0".into()),
            chmod: None }]);
    }

    #[test]
    fn copy_json_form() {
        assert_eq!(parse(r#"COPY ["a b", "dst"]"#), vec![Instruction::Copy {
            sources: vec!["a b".into()], dest: "dst".into(),
            from: None, chown: None, chmod: None }]);
    }

    #[test]
    fn copy_add_too_few_args_error() {
        // moby: COPY/ADD need >= 2 arguments.
        assert!(parse_err("COPY arg1").msg.contains("at least two"));
        assert!(parse_err("ADD arg1").msg.contains("at least two"));
    }

    #[test]
    fn add_with_chmod() {
        let i = parse("ADD --chmod=755 https://x/y.sh /usr/local/bin/y");
        assert_eq!(i, vec![Instruction::Add {
            sources: vec!["https://x/y.sh".into()],
            dest: "/usr/local/bin/y".into(),
            chown: None, chmod: Some("755".into()) }]);
    }

    #[test]
    fn cmd_entrypoint_workdir_user() {
        let i = parse("WORKDIR /app\nUSER 1000:1000\n\
                       ENTRYPOINT [\"/bin/app\"]\nCMD [\"--help\"]");
        assert_eq!(i, vec![
            Instruction::Workdir("/app".into()),
            Instruction::User("1000:1000".into()),
            Instruction::Entrypoint(Cmdline::Exec(vec!["/bin/app".into()])),
            Instruction::Cmd(Cmdline::Exec(vec!["--help".into()])),
        ]);
    }

    #[test]
    fn expose_volume_errors() {
        assert!(parse_err("EXPOSE").msg.contains("at least one"));
        assert!(parse_err("VOLUME").msg.contains("at least one"));
    }

    #[test]
    fn unknown_instruction_is_hard_error() {
        // moby: `unknown instruction: FOO`.
        let e = parse_err("FOO bar");
        assert!(e.msg.contains("unknown instruction"), "{}", e.msg);
    }

    #[test]
    fn known_but_unsupported_is_carried() {
        // MAINTAINER is a real Docker verb we don't act on — carried as
        // Unsupported, not an "unknown instruction" error.
        let i = parse("MAINTAINER someone@example.com");
        assert!(matches!(i[0], Instruction::Unsupported { .. }));
    }

    #[test]
    fn stopsignal_onbuild_healthcheck_parsed() {
        assert_eq!(parse("STOPSIGNAL SIGTERM"),
            vec![Instruction::Stopsignal("SIGTERM".into())]);
        assert_eq!(parse("ONBUILD RUN make"),
            vec![Instruction::Onbuild("RUN make".into())]);
        // HEALTHCHECK NONE disables an inherited check.
        assert_eq!(parse("HEALTHCHECK NONE"),
            vec![Instruction::Healthcheck(HealthcheckSpec {
                none: true, test: None, interval: None, timeout: None,
                start_period: None, start_interval: None, retries: None })]);
        // Options + shell-form CMD.
        assert_eq!(
            parse("HEALTHCHECK --interval=30s --retries=3 CMD curl -f http://localhost/"),
            vec![Instruction::Healthcheck(HealthcheckSpec {
                none: false,
                test: Some(Cmdline::Shell("curl -f http://localhost/".into())),
                interval: Some("30s".into()), timeout: None,
                start_period: None, start_interval: None, retries: Some(3) })]);
    }

    #[test]
    fn onbuild_and_healthcheck_errors() {
        assert!(parse_err("ONBUILD ONBUILD RUN x").msg.contains("Chaining"));
        assert!(parse_err("ONBUILD FROM x").msg.contains("isn't allowed"));
        assert!(parse_err("HEALTHCHECK --interval=30s curl x").msg.contains("CMD"));
    }

    #[test]
    fn label_multi() {
        assert_eq!(parse(r#"LABEL a=1 b="x y""#),
            vec![Instruction::Label(vec![
                ("a".into(), "1".into()), ("b".into(), "x y".into())])]);
    }

    #[test]
    fn expand_vars() {
        let mut m = HashMap::new();
        m.insert("VERSION".to_string(), "9".to_string());
        m.insert("EMPTY".to_string(), "".to_string());
        assert_eq!(expand("v$VERSION", &m), "v9");
        assert_eq!(expand("v${VERSION}-x", &m), "v9-x");
        assert_eq!(expand("${MISSING}", &m), "");
        assert_eq!(expand("${MISSING:-def}", &m), "def");
        assert_eq!(expand("${VERSION:+set}", &m), "set");
        assert_eq!(expand("${EMPTY:-fallback}", &m), "fallback");
        assert_eq!(expand("a$$b", &m), "a$b");
        assert_eq!(expand("a\\$b", &m), "a$b");
    }

    #[test]
    fn case_insensitive_verbs() {
        let i = parse("from alpine\nrun true");
        assert!(matches!(i[0], Instruction::From { .. }));
        assert!(matches!(i[1], Instruction::Run(_)));
    }
}
