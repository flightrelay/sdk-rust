//! [`run`]: a complete `main` for a [`DeviceDriver`].
//!
//! Requires the `main` feature. A binary is one line:
//!
//! ```no_run
//! # use flightrelay::driver::DeviceDriver;
//! # fn main_for<D: DeviceDriver>() -> std::process::ExitCode {
//! flightrelay::run::<D>()
//! # }
//! ```
//!
//! The command line is named flags only, in any order, with no positional
//! arguments and no subcommands:
//!
//! ```text
//! <program> [--config <path>] [--target <listen-addr | ws://host:port/frp>]
//!           [--set <key>=<value>]... [--log <info|debug|trace>] [--help]
//! ```
//!
//! - `--config <path>`: a TOML file holding the driver's config table at the
//!   top level, and the runner's own settings in an `[frp]` table (below).
//!   Without it the config starts from [`RunOptions::default_config`].
//! - `--set key=value` (repeatable): applied over the file, in order. The key
//!   is a dotted path into the config; the value is parsed as a TOML value,
//!   or taken as a string when it does not parse (`--set address=10.0.0.1:5100`,
//!   `--set club=PW`, `--set range=40`). A text that parses as another TOML
//!   type is quoted to be taken as a string (`--set label='"true"'`). A key
//!   the config does not have is refused, so a typo cannot go unnoticed; a
//!   key is known when setting it changes the config, so an optional field
//!   that is unset by default is settable. A file may carry keys the config
//!   ignores. `--set frp.<key>=<value>` sets a runner setting.
//! - `--target`: a `ws://` URL bridges (dials a hub, again whenever the link
//!   drops); `wss://` is refused; anything else is an address to listen on.
//!   Default: [`RunOptions::default_target`].
//! - `--log`: the lowest level of driver log lines to print. Default `info`.
//! - `--help`: usage, then every config key with its default.
//!
//! The runner's settings, in the `[frp]` table:
//!
//! - `actor_id`: the launch monitor actor's id on the bus. When the file has
//!   none, the runner generates one ([`new_actor_id`](crate::new_actor_id))
//!   and writes it into the file's `[frp]` table, so the actor keeps its id
//!   across restarts. The file is changed only when the edit reads back as
//!   the same config plus `actor_id`; a file whose `[frp]` table it cannot
//!   edit that way (dotted `frp.` keys, an inline `frp = { .. }`, a
//!   `[ frp ]` header) is left untouched, and the runner prints the id with
//!   the line to add. Without `--config` the id lasts for the run only.
//! - `raw_frames`: whether the actor publishes `shot.raw`. Default `false`.
//!
//! The config is resolved as defaults, then the file, then each `--set`, then
//! [`validate`](DriverConfig::validate). A bad flag, file, key or value prints
//! the problem and exits 2. A driver that gives up exits 1. Ctrl-C (and
//! SIGTERM on Unix) stops the server: the session is closed, the actor's
//! removal goes out, and the process exits 0.
//!
//! Output is one stderr line per event, prefixed with the program name:
//! listening, peers coming and going, the device connecting and being lost,
//! each driver log line, a line per shot and each change of the turn's
//! hints.

use std::io::Write;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value};

use crate::config::{ConfigError, DriverConfig};
use crate::driver::{DeviceDriver, DeviceEvent, Level, Stop};
use crate::{FrpBody, FrpServer, ServerConfig, ServerEvent, Transport};

/// The table of the config file that holds the runner's own settings.
const RUNNER_TABLE: &str = "frp";

/// What differs between binaries that run on [`run`].
pub struct RunOptions<D: DeviceDriver> {
    /// The config the file and `--set` flags apply over. Default:
    /// `D::Config::default`.
    pub default_config: fn() -> D::Config,
    /// Where to listen or whom to dial when `--target` is absent. Default
    /// `"0.0.0.0:5880"`.
    pub default_target: &'static str,
    /// The name printed in usage and as the prefix of each line. Default: the
    /// executable's name for [`run`] and [`run_with`], and the driver kind
    /// plus `-frp` for [`run_args`].
    pub program: Option<&'static str>,
}

impl<D: DeviceDriver> Default for RunOptions<D> {
    fn default() -> Self {
        Self {
            default_config: D::Config::default,
            default_target: "0.0.0.0:5880",
            program: None,
        }
    }
}

impl<D: DeviceDriver> std::fmt::Debug for RunOptions<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunOptions")
            .field("default_target", &self.default_target)
            .field("program", &self.program)
            .finish_non_exhaustive()
    }
}

/// Run `D` as a program: the arguments come from `std::env`. Never returns
/// before the process should exit.
#[must_use]
pub fn run<D: DeviceDriver>() -> ExitCode {
    run_with::<D>(RunOptions::default())
}

/// [`run`] with options.
#[must_use]
#[allow(clippy::needless_pass_by_value)] // the options belong to the program being run
pub fn run_with<D: DeviceDriver>(options: RunOptions<D>) -> ExitCode {
    let mut args = std::env::args_os().map(|a| a.to_string_lossy().into_owned());
    let program = args.next().map(|path| {
        std::path::Path::new(&path)
            .file_stem()
            .map_or(path.clone(), |s| s.to_string_lossy().into_owned())
    });
    run_with_program::<D>(program, args, &options)
}

/// [`run`] over explicit arguments, which exclude the program name.
#[must_use]
#[allow(clippy::needless_pass_by_value)] // the options belong to the program being run
pub fn run_args<D: DeviceDriver>(
    args: impl Iterator<Item = String>,
    options: RunOptions<D>,
) -> ExitCode {
    run_with_program::<D>(None, args, &options)
}

fn run_with_program<D: DeviceDriver>(
    argv0: Option<String>,
    args: impl Iterator<Item = String>,
    options: &RunOptions<D>,
) -> ExitCode {
    let program = options
        .program
        .map(str::to_owned)
        .or(argv0)
        .unwrap_or_else(|| format!("{}-frp", D::info(&(options.default_config)()).kind));
    let stop = Stop::new();
    let handler = stop.clone();
    if let Err(e) = ctrlc::set_handler(move || handler.stop()) {
        eprintln!("{program}: cannot install the Ctrl-C handler: {e}");
    }
    let stdout: Mutex<Box<dyn Write + Send>> = Mutex::new(Box::new(std::io::stdout()));
    let stderr: Arc<Mutex<Box<dyn Write + Send>>> =
        Arc::new(Mutex::new(Box::new(std::io::stderr())));
    let code = execute::<D>(
        &program,
        &args.collect::<Vec<_>>(),
        options,
        &stop,
        &stdout,
        &stderr,
    );
    ExitCode::from(code)
}

type Stream = Mutex<Box<dyn Write + Send>>;

fn say(stream: &Stream, line: &str) {
    let mut w = stream.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = writeln!(w, "{line}");
}

/// What the command line asked for.
#[derive(Debug, Default)]
struct Args {
    config: Option<String>,
    target: Option<String>,
    sets: Vec<String>,
    log: Option<Level>,
    help: bool,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_owned())),
            _ => (arg.as_str(), None),
        };
        let mut value = |name: &str| {
            inline
                .clone()
                .or_else(|| it.next().cloned())
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match flag {
            "--help" | "-h" => parsed.help = true,
            "--config" => parsed.config = Some(value("--config")?),
            "--target" => parsed.target = Some(value("--target")?),
            "--set" => parsed.sets.push(value("--set")?),
            "--log" => {
                let v = value("--log")?;
                parsed.log = Some(match v.as_str() {
                    "info" => Level::Info,
                    "debug" => Level::Debug,
                    "trace" => Level::Trace,
                    other => {
                        return Err(format!("--log: {other:?} is not info, debug or trace"));
                    }
                });
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(parsed)
}

fn usage(program: &str, default_target: &str) -> String {
    format!(
        "Usage: {program} [--config <path>] [--target <listen-addr | ws://host:port/frp>]\n\
         \x20      {pad} [--set <key>=<value>]... [--log <info|debug|trace>] [--help]\n\
         \n\
         Options:\n\
         \x20 --config <path>   TOML file with the driver's config at the top level and an [frp] table\n\
         \x20 --target <t>      an address to listen on, or a ws:// URL to dial (default {default_target})\n\
         \x20 --set key=value   set a config key over the file (frp.<key> for the runner); repeatable,\n\
         \x20                    dotted keys reach tables;\n\
         \x20                    a value that parses as another TOML type is quoted to be a string\n\
         \x20 --log <level>     lowest driver log level to print (default info)\n\
         \x20 --help            print this text",
        pad = " ".repeat(program.len()),
    )
}

/// The runner's own settings, from the `[frp]` table.
#[derive(Debug, Default, PartialEq)]
struct Runner {
    actor_id: Option<String>,
    raw_frames: bool,
}

impl Runner {
    fn from_map(map: &Map<String, Value>) -> Result<Self, ConfigError> {
        let mut runner = Self::default();
        for (key, value) in map {
            match (key.as_str(), value) {
                ("actor_id", Value::String(id)) if !id.is_empty() => {
                    runner.actor_id = Some(id.clone());
                }
                ("actor_id", _) => {
                    return Err(ConfigError::field(
                        "frp.actor_id",
                        "must be a non-empty string",
                    ));
                }
                ("raw_frames", Value::Bool(on)) => runner.raw_frames = *on,
                ("raw_frames", _) => {
                    return Err(ConfigError::field(
                        "frp.raw_frames",
                        "must be true or false",
                    ));
                }
                (other, _) => {
                    return Err(ConfigError::field(format!("frp.{other}"), "unknown key"));
                }
            }
        }
        Ok(runner)
    }
}

/// Resolves the config: defaults, then the file, then each `--set`, then
/// `validate`. The `[frp]` table and `frp.` keys go to the runner.
fn resolve<C: DriverConfig>(base: &C, args: &Args) -> Result<(C, Runner), ConfigError> {
    let mut map = base.to_map();
    let mut runner = Map::new();
    if let Some(path) = &args.config {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::new(format!("cannot read {path}: {e}")))?;
        let table: toml::Table = toml::from_str(&text)
            .map_err(|e| ConfigError::new(format!("{path}: {}", e.message())))?;
        if let Value::Object(mut file) = toml_to_json(&toml::Value::Table(table)) {
            match file.remove(RUNNER_TABLE) {
                Some(Value::Object(own)) => runner = own,
                Some(_) => {
                    return Err(ConfigError::field(RUNNER_TABLE, "must be a table"));
                }
                None => {}
            }
            merge(&mut map, file);
        }
    }
    for set in &args.sets {
        let (key, raw) = set
            .split_once('=')
            .ok_or_else(|| ConfigError::new(format!("--set {set:?}: expected key=value")))?;
        if let Some(own) = key.strip_prefix("frp.") {
            runner.insert(own.to_owned(), parse_value(raw));
            continue;
        }
        set_path(&mut map, key, parse_value(raw))?;
        check_known::<C>(&map, key)?;
    }
    let runner = Runner::from_map(&runner)?;
    Ok((C::from_map(map)?, runner))
}

/// Writes `actor_id` into the `[frp]` table of the TOML file at `path`: on
/// the line after the table's `[frp]` header, or in a new table at the end.
/// The rest of the file is kept as it is, line endings included.
///
/// The new text is read back before it is written: `frp.actor_id` must be
/// `actor_id` and every other key unchanged. When it is not (the table is
/// written with dotted keys, inline, as `[ frp ]`, or in any other shape this
/// edit does not handle), the file is left untouched and this returns
/// `Ok(false)`. The file is replaced through a temporary file in the same
/// directory, so a failed write leaves the old file whole.
fn save_actor_id(path: &str, actor_id: &str) -> std::io::Result<bool> {
    let text = std::fs::read_to_string(path)?;
    let Some(out) = with_actor_id(&text, actor_id) else {
        return Ok(false);
    };
    replace_file(std::path::Path::new(path), &out)?;
    Ok(true)
}

/// `text` with `actor_id = "<actor_id>"` added to its `[frp]` table, or
/// `None` when the result does not read back as `text` plus that one key.
fn with_actor_id(text: &str, actor_id: &str) -> Option<String> {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let line = format!("actor_id = {}", toml::Value::String(actor_id.to_owned()));
    let is_header = |l: &str| {
        l.trim().strip_prefix("[frp]").is_some_and(|rest| {
            let rest = rest.trim_start();
            rest.is_empty() || rest.starts_with('#')
        })
    };
    let mut out = String::with_capacity(text.len() + line.len() + 16);
    let mut placed = false;
    for l in text.split_inclusive('\n') {
        out.push_str(l);
        if !placed && is_header(l) {
            if !l.ends_with('\n') {
                out.push_str(eol);
            }
            out.push_str(&line);
            out.push_str(eol);
            placed = true;
        }
    }
    if !placed {
        if !out.is_empty() {
            if !out.ends_with('\n') {
                out.push_str(eol);
            }
            out.push_str(eol);
        }
        out.push_str("[frp]");
        out.push_str(eol);
        out.push_str(&line);
        out.push_str(eol);
    }
    adds_only_actor_id(text, &out, actor_id).then_some(out)
}

/// Whether `after` reads as `before` with `frp.actor_id = actor_id` added.
fn adds_only_actor_id(before: &str, after: &str, actor_id: &str) -> bool {
    let (Ok(before), Ok(mut after)) = (
        toml::from_str::<toml::Table>(before),
        toml::from_str::<toml::Table>(after),
    ) else {
        return false;
    };
    let Some(toml::Value::Table(frp)) = after.get_mut(RUNNER_TABLE) else {
        return false;
    };
    if frp
        .remove("actor_id")
        .and_then(|v| v.as_str().map(str::to_owned))
        .as_deref()
        != Some(actor_id)
    {
        return false;
    }
    if frp.is_empty() && !before.contains_key(RUNNER_TABLE) {
        after.remove(RUNNER_TABLE);
    }
    after == before
}

/// Replaces the file at `path` (a link's target, when it is a link) with
/// `text`, through a temporary file in the same directory and a rename. The
/// new file keeps the old one's permissions.
fn replace_file(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    let path = std::fs::canonicalize(path)?;
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let name = path
        .file_name()
        .map_or_else(|| "config".into(), |n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let written = std::fs::write(&tmp, text)
        .and_then(|()| std::fs::metadata(&path))
        .and_then(|meta| std::fs::set_permissions(&tmp, meta.permissions()))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Lays `over` on `base`, merging tables key by key.
fn merge(base: &mut Map<String, Value>, over: Map<String, Value>) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(Value::Object(b)), Value::Object(o)) => merge(b, o),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// Refuses a `--set` key the config does not have. `map` holds the value just
/// set at `path`. The key is known when the config read from `map`, written
/// out with every field present, has it, or, for a field serde renames, when
/// leaving the key out changes the config. A map that does not read as a
/// config is left to [`DriverConfig::from_map`], which names the field at
/// fault.
fn check_known<C: DriverConfig>(map: &Map<String, Value>, path: &str) -> Result<(), ConfigError> {
    let Ok(with) = serde_json::from_value::<C>(Value::Object(map.clone())) else {
        return Ok(());
    };
    let with = crate::config::listed(&with);
    if has_path(&with, path) {
        return Ok(());
    }
    let mut without = map.clone();
    remove_path(&mut without, path);
    match serde_json::from_value::<C>(Value::Object(without)) {
        Ok(without) if crate::config::listed(&without) != with => Ok(()),
        _ => Err(ConfigError::field(path, "unknown key")),
    }
}

fn has_path(value: &Value, path: &str) -> bool {
    path.split('.')
        .try_fold(value, |node, part| node.as_object()?.get(part))
        .is_some()
}

fn remove_path(map: &mut Map<String, Value>, path: &str) {
    match path.split_once('.') {
        Some((head, rest)) => {
            if let Some(Value::Object(inner)) = map.get_mut(head) {
                remove_path(inner, rest);
            }
        }
        None => {
            map.remove(path);
        }
    }
}

fn set_path(map: &mut Map<String, Value>, path: &str, value: Value) -> Result<(), ConfigError> {
    let mut parts: Vec<&str> = path.split('.').collect();
    let Some(last) = parts.pop().filter(|l| !l.is_empty()) else {
        return Err(ConfigError::new(format!("--set {path:?}: empty key")));
    };
    let mut table = map;
    for part in parts {
        let entry = table
            .entry(part.to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        table = entry
            .as_object_mut()
            .ok_or_else(|| ConfigError::field(path, format!("{part} is not a table")))?;
    }
    table.insert(last.to_owned(), value);
    Ok(())
}

/// A `--set` value: a TOML value when it parses as one, else the text.
fn parse_value(raw: &str) -> Value {
    let parsed = toml::from_str::<toml::Table>(&format!("v = {raw}"))
        .ok()
        .and_then(|mut t| (t.len() == 1).then(|| t.remove("v")).flatten());
    parsed.map_or_else(|| Value::String(raw.to_owned()), |v| toml_to_json(&v))
}

pub(crate) fn toml_to_json(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => {
            serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number)
        }
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(a) => Value::Array(a.iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => Value::Object(
            t.iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect(),
        ),
    }
}

fn json_to_toml(value: &Value) -> Option<toml::Value> {
    Some(match value {
        Value::Null => return None,
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Number(n) => n.as_i64().map_or_else(
            || n.as_f64().map(toml::Value::Float),
            |i| Some(toml::Value::Integer(i)),
        )?,
        Value::String(s) => toml::Value::String(s.clone()),
        Value::Array(a) => toml::Value::Array(a.iter().filter_map(json_to_toml).collect()),
        Value::Object(o) => toml::Value::Table(
            o.iter()
                .filter_map(|(k, v)| json_to_toml(v).map(|v| (k.clone(), v)))
                .collect(),
        ),
    })
}

/// A config as `--help` lists it: each key with its value, a key whose value
/// is unset as a comment, tables as `[sections]`.
pub(crate) fn render_toml(value: &Value) -> String {
    let mut out = String::new();
    if let Value::Object(map) = value {
        render_table("", map, &mut out);
    }
    out
}

fn render_key(key: &str) -> String {
    if !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        key.to_owned()
    } else {
        toml::Value::String(key.to_owned()).to_string()
    }
}

fn render_table(path: &str, map: &Map<String, Value>, out: &mut String) {
    use std::fmt::Write;
    for (key, value) in map {
        match value {
            Value::Object(_) => {}
            Value::Null => {
                let _ = writeln!(out, "# {} = (unset)", render_key(key));
            }
            other => {
                if let Some(v) = json_to_toml(other) {
                    let _ = writeln!(out, "{} = {v}", render_key(key));
                }
            }
        }
    }
    for (key, value) in map {
        if let Value::Object(inner) = value {
            let path = if path.is_empty() {
                render_key(key)
            } else {
                format!("{path}.{}", render_key(key))
            };
            let _ = writeln!(out, "\n[{path}]");
            render_table(&path, inner, out);
        }
    }
}

/// The testable body of [`run`]: returns the exit code.
fn execute<D: DeviceDriver>(
    program: &str,
    args: &[String],
    options: &RunOptions<D>,
    stop: &Stop,
    stdout: &Stream,
    stderr: &Arc<Stream>,
) -> u8 {
    let args = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            say(stderr, &format!("{program}: {e}"));
            say(stderr, &usage(program, options.default_target));
            return 2;
        }
    };
    let base = (options.default_config)();
    if args.help {
        say(stdout, &usage(program, options.default_target));
        say(stdout, "\nConfig keys and their defaults:\n");
        say(stdout, base.help_toml_of().trim_end());
        say(
            stdout,
            "\n[frp]\n# actor_id = (unset: generated, and saved to the --config file)\nraw_frames = false",
        );
        return 0;
    }
    let (config, runner) = match resolve(&base, &args) {
        Ok(c) => c,
        Err(e) => {
            say(stderr, &format!("{program}: {e}"));
            return 2;
        }
    };
    let driver = match D::new(config) {
        Ok(d) => d,
        Err(e) => {
            say(stderr, &format!("{program}: {e}"));
            return 2;
        }
    };
    let target = args
        .target
        .as_deref()
        .unwrap_or(options.default_target)
        .to_owned();
    if target
        .get(..6)
        .is_some_and(|s| s.eq_ignore_ascii_case("wss://"))
    {
        say(
            stderr,
            &format!("{program}: --target {target:?}: wss:// is not supported; use ws://"),
        );
        return 2;
    }
    let transport = if target.starts_with("ws://") {
        Transport::connect(target)
    } else {
        Transport::listen(target)
    };
    let actor_id = runner
        .actor_id
        .unwrap_or_else(|| new_actor_id(program, args.config.as_deref(), stderr));
    let log = args.log.unwrap_or(Level::Info);
    let mut server_config = ServerConfig::new(transport, actor_id);
    server_config.log_level = log;
    server_config.raw_frames = runner.raw_frames;

    let out = Arc::clone(stderr);
    let prefix = program.to_owned();
    let result = FrpServer::new(driver, server_config)
        .on_event(move |event| {
            if let Some(line) = describe(event, log) {
                say(&out, &format!("{prefix}: {line}"));
            }
        })
        .run(stop);
    match result {
        Ok(()) => 0,
        Err(e) => {
            say(stderr, &format!("{program}: {e}"));
            1
        }
    }
}

/// A new actor id, saved to the config file at `path` when there is one.
fn new_actor_id(program: &str, path: Option<&str>, stderr: &Stream) -> String {
    let id = crate::new_actor_id();
    let line = match path {
        Some(path) => match save_actor_id(path, &id) {
            Ok(true) => format!("{program}: new actor id {id}, saved to {path}"),
            Ok(false) => format!(
                "{program}: new actor id {id} for this run; {path} was left as it is: \
                 add actor_id = \"{id}\" to its [frp] table to keep it"
            ),
            Err(e) => format!(
                "{program}: new actor id {id} for this run; cannot save it to {path} ({e}): \
                 add actor_id = \"{id}\" to its [frp] table to keep it"
            ),
        },
        None => format!("{program}: new actor id {id} for this run; use --config to keep one"),
    };
    say(stderr, &line);
    id
}

/// A delay as the output prints it: whole seconds, else milliseconds below a
/// second, else tenths of a second.
fn delay_text(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms.is_multiple_of(1000) {
        format!("{} s", ms / 1000)
    } else if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", d.as_secs_f64())
    }
}

/// One output line for a server event, or none.
fn describe(event: &ServerEvent<'_>, log: Level) -> Option<String> {
    let at = |addr: Option<std::net::SocketAddr>| addr.map_or(String::new(), |a| format!(" ({a})"));
    let opt = |v: Option<String>| v.unwrap_or_else(|| "-".into());
    Some(match event {
        ServerEvent::Listening(addr) => format!("listening on {addr}"),
        ServerEvent::PeerConnected { addr } => format!("peer connected{}", at(*addr)),
        ServerEvent::PeerLeft { addr, reason } => format!("peer left{}: {reason}", at(*addr)),
        ServerEvent::DeviceConnected(identity) => {
            format!("device connected: {}", identity.label())
        }
        ServerEvent::DeviceLost { error, retry_in } => {
            let why = error.map_or_else(|| "stopped".to_owned(), ToString::to_string);
            match retry_in {
                Some(d) => format!("device lost: {why}; retrying in {}", delay_text(*d)),
                None => format!("device lost: {why}; giving up"),
            }
        }
        ServerEvent::Hints(hints) => format!(
            "turn: hand={} club={} mode={}",
            opt(hints.handedness.map(|h| h.to_string())),
            opt(hints.club.map(|c| c.to_string())),
            opt(hints.mode.map(|m| m.to_string())),
        ),
        ServerEvent::Device(DeviceEvent::Log(level, line)) => format!("{level}: {line}"),
        ServerEvent::Device(DeviceEvent::Phase(phase)) if log <= Level::Debug => {
            format!("phase: {phase}")
        }
        ServerEvent::Device(DeviceEvent::Frp(FrpBody::ShotLaunch(m))) => {
            use std::fmt::Write;
            let launch = &m.launch;
            let mut line = format!("shot {}:", m.key.shot_number);
            if let Some(v) = launch.launch_speed {
                let _ = write!(line, " ball {:.1} mph", v.as_mph());
            }
            if let Some(a) = launch.launch_elevation {
                let _ = write!(line, ", launch {a:.1} deg");
            }
            if let Some(s) = launch.total_spin_rpm.or(launch.backspin_rpm) {
                let _ = write!(line, ", spin {s} rpm");
            }
            line
        }
        ServerEvent::Device(DeviceEvent::Frp(FrpBody::BusAlert(alert))) => {
            format!("alert ({}): {}", alert.severity, alert.message)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
