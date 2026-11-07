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
//!   top level. Without it the config starts from
//!   [`RunOptions::default_config`].
//! - `--set key=value` (repeatable): applied over the file, in order. The key
//!   is a dotted path into the config; the value is parsed as a TOML value,
//!   or taken as a string when it does not parse (`--set address=10.0.0.1:5100`,
//!   `--set club=PW`, `--set range=40`). A text that parses as another TOML
//!   type is quoted to be taken as a string (`--set label='"true"'`). A key
//!   the config does not have is refused, so a typo cannot go unnoticed; a
//!   key is known when setting it changes the config, so an optional field
//!   that is unset by default is settable. A file may carry keys the config
//!   ignores.
//! - `--target`: a `ws://` URL bridges (dials a host, again whenever the link
//!   drops); `wss://` is refused; anything else is an address to listen on.
//!   Default: [`RunOptions::default_target`].
//! - `--log`: the lowest level of driver log lines to print. Default `info`.
//! - `--help`: usage, then every config key with its default.
//!
//! The config is resolved as defaults, then the file, then each `--set`, then
//! [`validate`](DriverConfig::validate). A bad flag, file, key or value prints
//! the problem and exits 2. A driver that gives up exits 1. Ctrl-C (and
//! SIGTERM on Unix) stops the server: the session is closed and the process
//! exits 0.
//!
//! Output is one stderr line per event, prefixed with the program name:
//! listening, controllers coming and going, the device connecting and being
//! lost, each driver log line, a line per shot and each command.

use std::io::Write;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value};

use crate::config::{ConfigError, DriverConfig};
use crate::driver::{DeviceDriver, DeviceEvent, Level, Stop};
use crate::{FrpEvent, FrpServer, ServerConfig, ServerEvent, Transport};

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
         \x20 --config <path>   TOML file with the driver's config at the top level\n\
         \x20 --target <t>      an address to listen on, or a ws:// URL to dial (default {default_target})\n\
         \x20 --set key=value   set a config key over the file; repeatable, dotted keys reach tables;\n\
         \x20                    a value that parses as another TOML type is quoted to be a string\n\
         \x20 --log <level>     lowest driver log level to print (default info)\n\
         \x20 --help            print this text",
        pad = " ".repeat(program.len()),
    )
}

/// Resolves the config: defaults, then the file, then each `--set`, then
/// `validate`.
fn resolve<C: DriverConfig>(base: &C, args: &Args) -> Result<C, ConfigError> {
    let mut map = base.to_map();
    if let Some(path) = &args.config {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::new(format!("cannot read {path}: {e}")))?;
        let table: toml::Table = toml::from_str(&text)
            .map_err(|e| ConfigError::new(format!("{path}: {}", e.message())))?;
        if let Value::Object(file) = toml_to_json(&toml::Value::Table(table)) {
            merge(&mut map, file);
        }
    }
    for set in &args.sets {
        let (key, raw) = set
            .split_once('=')
            .ok_or_else(|| ConfigError::new(format!("--set {set:?}: expected key=value")))?;
        set_path(&mut map, key, parse_value(raw))?;
        check_known::<C>(&map, key)?;
    }
    C::from_map(map)
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
        return 0;
    }
    let config = match resolve(&base, &args) {
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
    let log = args.log.unwrap_or(Level::Info);
    let mut server_config = ServerConfig::new(transport);
    server_config.log_level = log;

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
    let name = |peer: Option<&str>| peer.map_or(String::new(), |p| format!(" ({p})"));
    Some(match event {
        ServerEvent::Listening(addr) => format!("listening on {addr}"),
        ServerEvent::ControllerConnected { peer } => {
            format!("controller connected{}", name(*peer))
        }
        ServerEvent::ControllerLeft { peer, reason } => {
            format!("controller left{}: {reason}", name(*peer))
        }
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
        ServerEvent::Command(cmd) => format!(
            "command: mode={} hand={} club={}",
            cmd.mode.map_or("-".into(), |v| v.to_string()),
            cmd.handed.map_or("-", |h| match h {
                crate::Handedness::Right => "rh",
                crate::Handedness::Left => "lh",
            }),
            cmd.club.map_or("-".into(), |c| {
                serde_json::to_value(c)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default()
            }),
        ),
        ServerEvent::Device(DeviceEvent::Log(level, line)) => format!("{level}: {line}"),
        ServerEvent::Device(DeviceEvent::Phase(phase)) if log <= Level::Debug => {
            format!("phase: {phase}")
        }
        ServerEvent::Device(DeviceEvent::Frp(FrpEvent::Launch { key, launch })) => {
            use std::fmt::Write;
            let mut line = format!("shot {}:", key.shot_number);
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
        ServerEvent::Device(DeviceEvent::Frp(FrpEvent::Alert { severity, message })) => {
            format!("alert ({severity}): {message}")
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
