#![allow(clippy::float_cmp)]

use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::*;
use crate::config::is_default;
use crate::driver::{
    ConnectCtx, DeviceIdentity, DeviceSession, DriverInfo, Events, NoAction, Stopped,
};
use crate::{EndpointConfig, FrpEndpoint, Role, Source};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Tuning {
    #[serde(skip_serializing_if = "is_default")]
    gain: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Cfg {
    #[serde(skip_serializing_if = "is_default")]
    address: Option<String>,
    #[serde(skip_serializing_if = "is_default")]
    club: String,
    #[serde(skip_serializing_if = "is_default", alias = "old_count")]
    count: u32,
    /// Optional, and skipped while unset by a predicate that is not `is_default`.
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    /// Optional table.
    #[serde(skip_serializing_if = "Option::is_none")]
    extra: Option<Tuning>,
    #[serde(skip_serializing_if = "is_default")]
    give_up: bool,
    /// Logs at every level on connect, then stops the server.
    #[serde(skip_serializing_if = "is_default")]
    chatty: bool,
    #[serde(skip_serializing_if = "is_default")]
    tuning: Tuning,
}

impl DriverConfig for Cfg {
    fn example() -> Self {
        Self {
            note: Some(String::new()),
            extra: Some(Tuning::default()),
            ..Self::default()
        }
    }
    fn validate(&self) -> Result<(), ConfigError> {
        if self.count > 100 {
            return Err(ConfigError::field("count", "must be 0 to 100"));
        }
        Ok(())
    }
}

thread_local! {
    /// The config the last `Fake::new` on this thread was built with.
    static BUILT: RefCell<Option<Cfg>> = const { RefCell::new(None) };
}

#[derive(Debug, thiserror::Error)]
enum FakeError {
    #[error("stopped")]
    Stopped,
    #[error("no device")]
    NoDevice,
}

impl From<Stopped> for FakeError {
    fn from(_: Stopped) -> Self {
        Self::Stopped
    }
}

struct Fake(Cfg);
struct FakeSession(DeviceIdentity);

impl DeviceDriver for Fake {
    type Config = Cfg;
    type Error = FakeError;
    type Action = NoAction;
    type Session = FakeSession;

    fn info(_: &Cfg) -> DriverInfo {
        DriverInfo::new("fake", "Fake", Source::Virtual)
    }
    fn new(config: Cfg) -> Result<Self, ConfigError> {
        config.validate()?;
        BUILT.with(|b| *b.borrow_mut() = Some(config.clone()));
        Ok(Self(config))
    }
    fn config(&self) -> &Cfg {
        &self.0
    }
    fn apply_config(&mut self, new: &Cfg) {
        self.0 = new.clone();
    }
    fn connect(&mut self, ctx: &mut ConnectCtx<'_>) -> Result<FakeSession, FakeError> {
        ctx.tick()?;
        if self.0.give_up {
            return Err(FakeError::NoDevice);
        }
        if self.0.chatty {
            ctx.out.log(Level::Trace, "chatty trace");
            ctx.out.log(Level::Debug, "chatty debug");
            ctx.out.log(Level::Info, "chatty info");
            ctx.out.log(Level::Warn, "chatty warn");
            ctx.stop.stop();
        }
        Ok(FakeSession(DeviceIdentity::new("Fake-1")))
    }
    fn retry_delay(
        &mut self,
        _: &SessionEnd<'_, FakeError>,
        _: &mut crate::driver::Backoff,
        _: &mut Events<'_>,
    ) -> Option<Duration> {
        None
    }
}

impl DeviceSession for FakeSession {
    type Driver = Fake;
    fn poll(&mut self, timeout: Duration, _: &mut Events<'_>) -> Result<(), FakeError> {
        std::thread::sleep(timeout.min(Duration::from_millis(10)));
        Ok(())
    }
    fn identity(&self) -> &DeviceIdentity {
        &self.0
    }
    fn close(self, _: &mut Events<'_>) -> Result<(), FakeError> {
        Ok(())
    }
}

use crate::driver::SessionEnd;

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buf {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

struct Run {
    code: u8,
    out: String,
    err: String,
    built: Option<Cfg>,
}

fn go(args: &[&str], stop: &Stop) -> Run {
    let out = Buf::default();
    let err = Buf::default();
    let stdout: Stream = Mutex::new(Box::new(out.clone()));
    let stderr: Arc<Stream> = Arc::new(Mutex::new(Box::new(err.clone())));
    BUILT.with(|b| *b.borrow_mut() = None);
    let options = RunOptions::<Fake> {
        default_target: "127.0.0.1:0",
        ..RunOptions::default()
    };
    let args: Vec<String> = args.iter().map(|&s| s.to_owned()).collect();
    let code = execute::<Fake>("fake-frp", &args, &options, stop, &stdout, &stderr);
    Run {
        code,
        out: out.text(),
        err: err.text(),
        built: BUILT.with(|b| b.borrow().clone()),
    }
}

/// Runs with a stop that is already set, so a good command line exits 0 once
/// the server is up.
fn go_stopped(args: &[&str]) -> Run {
    let stop = Stop::new();
    stop.stop();
    go(args, &stop)
}

fn temp_file(name: &str, text: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("flightrelay-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

#[test]
fn the_defaults_apply_without_flags() {
    let run = go_stopped(&[]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(run.built, Some(Cfg::default()));
    assert!(
        run.err.contains("fake-frp: listening on 127.0.0.1:"),
        "{}",
        run.err
    );
}

#[test]
fn a_file_is_laid_over_the_defaults_and_sets_over_the_file() {
    let file = temp_file(
        "a.toml",
        "address = \"10.0.0.1:5100\"\ncount = 5\nclub = \"7I\"\n[tuning]\ngain = 0.5\n",
    );
    let run = go_stopped(&["--config", file.to_str().unwrap()]);
    assert_eq!(run.code, 0, "{}", run.err);
    let built = run.built.unwrap();
    assert_eq!(built.address.as_deref(), Some("10.0.0.1:5100"));
    assert_eq!(
        (built.count, built.club.as_str(), built.tuning.gain),
        (5, "7I", 0.5)
    );

    let run = go_stopped(&[
        "--set",
        "club=PW",
        "--config",
        file.to_str().unwrap(),
        "--set",
        "count=9",
        "--set",
        "count=11",
        "--set",
        "tuning.gain=0.25",
        "--set",
        "address=192.168.2.1:5100",
    ]);
    assert_eq!(run.code, 0, "{}", run.err);
    let built = run.built.unwrap();
    assert_eq!(
        built.club, "PW",
        "a --set before --config still lands after the file"
    );
    assert_eq!(built.count, 11, "the last --set of a key wins");
    assert_eq!(built.tuning.gain, 0.25);
    assert_eq!(built.address.as_deref(), Some("192.168.2.1:5100"));
}

#[test]
fn flags_take_a_value_after_a_space_or_an_equals_sign() {
    let run = go_stopped(&["--set=count=3", "--target=127.0.0.1:0", "--log=debug"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(run.built.unwrap().count, 3);
}

#[test]
fn values_parse_as_toml_or_fall_back_to_text() {
    assert_eq!(parse_value("40"), Value::from(40));
    assert_eq!(parse_value("0.25"), Value::from(0.25));
    assert_eq!(parse_value("true"), Value::Bool(true));
    assert_eq!(parse_value("\"a b\""), Value::from("a b"));
    assert_eq!(parse_value("PW"), Value::from("PW"));
    assert_eq!(
        parse_value("192.168.2.1:5100"),
        Value::from("192.168.2.1:5100")
    );
    assert_eq!(parse_value("[1, 2]"), serde_json::json!([1, 2]));
    assert_eq!(parse_value("1 = 2"), Value::from("1 = 2"));
}

#[test]
fn a_refused_config_exits_2_and_names_the_key() {
    let run = go_stopped(&["--set", "count=500"]);
    assert_eq!(run.code, 2);
    assert!(
        run.err.contains("fake-frp: count: must be 0 to 100"),
        "{}",
        run.err
    );
    assert!(run.built.is_none(), "the driver is not built");

    let run = go_stopped(&["--set", "count=abc"]);
    assert_eq!(run.code, 2);
    assert!(run.err.contains("count: "), "{}", run.err);

    let run = go_stopped(&["--set", "nokey"]);
    assert_eq!(run.code, 2);
    assert!(run.err.contains("expected key=value"), "{}", run.err);

    let run = go_stopped(&["--set", "count.x=1"]);
    assert_eq!(run.code, 2, "{}", run.err);
}

#[test]
fn a_set_key_the_config_does_not_have_exits_2_and_names_it() {
    for (key, named) in [
        ("nope", "nope"),
        ("rnage", "rnage"),
        ("tuning.nope", "tuning.nope"),
        ("nope.deeper", "nope.deeper"),
    ] {
        let run = go_stopped(&["--set", &format!("{key}=1")]);
        assert_eq!(run.code, 2, "{key}: {}", run.err);
        assert!(
            run.err.contains(&format!("fake-frp: {named}: unknown key")),
            "{key}: {}",
            run.err
        );
        assert!(run.built.is_none(), "{key}: the driver is not built");
    }
    // Every key of the config is accepted, a key whose default is unset too.
    let run = go_stopped(&[
        "--set",
        "address=10.0.0.1:5100",
        "--set",
        "tuning.gain=2",
        "--set",
        "club=PW",
    ]);
    assert_eq!(run.code, 0, "{}", run.err);
    // A file may carry keys the config ignores.
    let file = temp_file(
        "extra.toml",
        "count = 3
something_else = 1
",
    );
    let run = go_stopped(&["--config", file.to_str().unwrap()]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(run.built.unwrap().count, 3);
}

#[test]
fn an_optional_field_can_be_set_and_read_back() {
    let run = go_stopped(&["--set", "note=hello world"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(run.built.unwrap().note.as_deref(), Some("hello world"));

    // A value equal to the field type's default is still a set value.
    let run = go_stopped(&["--set", "note=\"\"", "--set", "address=\"\""]);
    assert_eq!(run.code, 0, "{}", run.err);
    let built = run.built.unwrap();
    assert_eq!(built.note.as_deref(), Some(""));
    assert_eq!(built.address.as_deref(), Some(""));

    // A key of an optional table, which is unset by default.
    let run = go_stopped(&["--set", "extra.gain=2.5"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(run.built.unwrap().extra, Some(Tuning { gain: 2.5 }));

    // A file's value and a `--set` over it.
    let file = temp_file("note.toml", "note = \"from the file\"\n");
    let run = go_stopped(&["--config", file.to_str().unwrap()]);
    assert_eq!(run.built.unwrap().note.as_deref(), Some("from the file"));
    let run = go_stopped(&["--config", file.to_str().unwrap(), "--set", "note=over"]);
    assert_eq!(run.built.unwrap().note.as_deref(), Some("over"));

    // A name serde accepts as an alias is a key, too.
    let run = go_stopped(&["--set", "old_count=5"]);
    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(run.built.unwrap().count, 5);

    // Unknown keys still exit 2, next to optional ones.
    for key in ["nope", "extra.nope", "tuning.nope"] {
        let run = go_stopped(&["--set", "note=x", "--set", &format!("{key}=1")]);
        assert_eq!(run.code, 2, "{key}: {}", run.err);
        assert!(
            run.err.contains(&format!("fake-frp: {key}: unknown key")),
            "{key}: {}",
            run.err
        );
    }

    // A wrong type on an optional field is the field's error.
    let run = go_stopped(&["--set", "extra.gain=abc"]);
    assert_eq!(run.code, 2);
    assert!(run.err.contains("extra.gain: "), "{}", run.err);
}

#[test]
fn an_error_in_a_nested_value_names_the_dotted_key() {
    let run = go_stopped(&["--set", "tuning.gain=abc"]);
    assert_eq!(run.code, 2);
    assert!(run.err.contains("fake-frp: tuning.gain: "), "{}", run.err);
}

#[test]
fn a_wss_target_exits_2_up_front() {
    for target in ["wss://127.0.0.1:1/frp", "WSS://127.0.0.1:1/frp"] {
        let run = go_stopped(&["--target", target]);
        assert_eq!(run.code, 2, "{target}: {}", run.err);
        assert!(
            run.err.contains("wss:// is not supported; use ws://"),
            "{}",
            run.err
        );
        assert!(!run.err.contains("listening"), "{}", run.err);
    }
}

#[test]
fn a_bad_file_exits_2() {
    let run = go_stopped(&["--config", "/nonexistent/flightrelay-test.toml"]);
    assert_eq!(run.code, 2);
    assert!(run.err.contains("cannot read"), "{}", run.err);
    let file = temp_file("bad.toml", "count = = 1\n");
    let run = go_stopped(&["--config", file.to_str().unwrap()]);
    assert_eq!(run.code, 2);
    assert!(run.err.contains("bad.toml"), "{}", run.err);
}

#[test]
fn unknown_flags_and_missing_values_print_usage_and_exit_2() {
    for args in [
        &["--bogus"][..],
        &["positional"],
        &["--config"],
        &["--set"],
        &["--target"],
        &["--log", "loud"],
        &["--log"],
    ] {
        let run = go_stopped(args);
        assert_eq!(run.code, 2, "{args:?}");
        assert!(run.err.contains("Usage: fake-frp"), "{args:?}: {}", run.err);
        assert!(run.out.is_empty(), "{args:?}");
    }
}

#[test]
fn help_lists_every_key_with_its_default_and_exits_0() {
    let run = go_stopped(&["--help"]);
    assert_eq!(run.code, 0);
    assert!(run.err.is_empty(), "{}", run.err);
    for line in [
        "Usage: fake-frp",
        "--config <path>",
        "--set key=value",
        "--log <level>",
        "(default 127.0.0.1:0)",
        "# address = (unset)",
        "# note = (unset)",
        "[extra]",
        "# gain = (unset)",
        "club = \"\"",
        "count = 0",
        "give_up = false",
        "[tuning]",
        "gain = 0.0",
        "quoted to be a string",
    ] {
        assert!(run.out.contains(line), "missing {line:?} in:\n{}", run.out);
    }
    assert!(run.built.is_none(), "help does not build the driver");
}

#[test]
fn help_shows_the_defaults_of_the_options() {
    let out = Buf::default();
    let stdout: Stream = Mutex::new(Box::new(out.clone()));
    let stderr: Arc<Stream> = Arc::new(Mutex::new(Box::new(Buf::default())));
    let options = RunOptions::<Fake> {
        default_config: || Cfg {
            count: 7,
            ..Cfg::default()
        },
        ..RunOptions::default()
    };
    let code = execute::<Fake>(
        "fake-frp",
        &["-h".to_owned()],
        &options,
        &Stop::new(),
        &stdout,
        &stderr,
    );
    assert_eq!(code, 0);
    assert!(out.text().contains("count = 7"), "{}", out.text());
    assert!(out.text().contains("(default 0.0.0.0:5880)"));
}

#[test]
fn the_config_helpers_round_trip_through_toml_and_maps() {
    let config = Cfg::from_toml_str("count = 4\n[tuning]\ngain = 2\n").unwrap();
    assert_eq!((config.count, config.tuning.gain), (4, 2.0));
    assert_eq!(
        Cfg::from_toml_str("count = 400").unwrap_err().to_string(),
        "count: must be 0 to 100"
    );
    assert!(Cfg::from_toml_str("count = =").is_err());
    assert!(
        Cfg::from_toml_str("count = \"x\"")
            .unwrap_err()
            .field
            .as_deref()
            == Some("count")
    );
    assert_eq!(
        Cfg::default().to_map().len(),
        0,
        "a default config writes nothing"
    );
    let map = config.to_map();
    assert_eq!(Cfg::from_map(map).unwrap(), config);
    let help = Cfg::help_toml();
    assert!(
        help.contains("count = 0") && help.contains("[tuning]"),
        "{help}"
    );
    assert_eq!(
        Cfg::default().to_map().len(),
        0,
        "listing defaults for --help leaves no trace behind"
    );
}

#[test]
fn a_driver_that_gives_up_exits_1() {
    let run = go(&["--set", "give_up=true"], &Stop::new());
    assert_eq!(run.code, 1);
    assert!(
        run.err.contains("device lost: no device; giving up"),
        "{}",
        run.err
    );
    assert!(run.err.contains("driver gave up: no device"), "{}", run.err);
}

#[test]
fn a_ws_target_dials_and_anything_else_listens() {
    // A controller that listens: the server dials it.
    let mut controller = FrpEndpoint::open(EndpointConfig::new(
        Role::Controller,
        crate::Transport::listen("127.0.0.1:0"),
    ))
    .unwrap();
    let addr = controller.local_addr().unwrap().unwrap();
    let stop = Stop::new();
    let runner_stop = stop.clone();
    let url = format!("ws://{addr}/frp");
    let runner = std::thread::spawn(move || go(&["--target", &url], &runner_stop));
    let conn = controller.establish().expect("the server dialed in");
    assert_eq!(conn.role(), Role::Controller);
    stop.stop();
    let run = runner.join().unwrap();
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(
        !run.err.contains("listening on"),
        "a bridge listens nowhere: {}",
        run.err
    );

    // A listen address: it reports where.
    let run = go_stopped(&["--target", "127.0.0.1:0"]);
    assert!(run.err.contains("listening on 127.0.0.1:"), "{}", run.err);
}

#[test]
fn log_lines_below_the_level_are_not_printed() {
    let lines = |level: &str| {
        let run = go(&["--set", "chatty=true", "--log", level], &Stop::new());
        assert_eq!(run.code, 0, "{level}: {}", run.err);
        run.err
    };
    let info = lines("info");
    assert!(info.contains("fake-frp: info: chatty info"), "{info}");
    assert!(info.contains("fake-frp: warn: chatty warn"), "{info}");
    assert!(!info.contains("chatty debug"), "{info}");
    assert!(!info.contains("chatty trace"), "{info}");
    let debug = lines("debug");
    assert!(debug.contains("fake-frp: debug: chatty debug"), "{debug}");
    assert!(!debug.contains("chatty trace"), "{debug}");
    let trace = lines("trace");
    assert!(trace.contains("fake-frp: trace: chatty trace"), "{trace}");
}

#[test]
fn describe_prints_the_log_lines_it_is_given() {
    let info = describe(
        &ServerEvent::Device(&DeviceEvent::Log(Level::Debug, "detail".into())),
        Level::Info,
    );
    // The server and the driver's sink do the filtering; `describe` prints
    // what it is given.
    assert_eq!(info.as_deref(), Some("debug: detail"));
    assert_eq!(
        describe(
            &ServerEvent::Device(&DeviceEvent::Phase("arming")),
            Level::Info
        ),
        None
    );
    assert_eq!(
        describe(
            &ServerEvent::Device(&DeviceEvent::Phase("arming")),
            Level::Debug
        )
        .as_deref(),
        Some("phase: arming")
    );
}

#[test]
fn delays_print_in_the_unit_that_keeps_them_visible() {
    assert_eq!(delay_text(Duration::from_millis(50)), "50 ms");
    assert_eq!(delay_text(Duration::from_secs(2)), "2 s");
    assert_eq!(delay_text(Duration::from_millis(1500)), "1.5 s");
    let lost = ServerEvent::DeviceLost {
        error: None,
        retry_in: Some(Duration::from_millis(50)),
    };
    assert_eq!(
        describe(&lost, Level::Info).as_deref(),
        Some("device lost: stopped; retrying in 50 ms")
    );
}

#[test]
fn output_lines_describe_shots_and_commands() {
    use crate::{Club, DetectionMode, Launch, ShotKey, Velocity};
    let launch = Launch {
        launch_speed: Some(Velocity::MetersPerSecond(67.056)),
        launch_elevation: Some(14.25),
        total_spin_rpm: Some(3231),
        ..Launch::default()
    };
    let event = DeviceEvent::Frp(FrpEvent::launch(ShotKey::new("id", 7), launch));
    assert_eq!(
        describe(&ServerEvent::Device(&event), Level::Info).as_deref(),
        Some("shot 7: ball 150.0 mph, launch 14.2 deg, spin 3231 rpm")
    );
    let cmd = crate::driver::DeviceCommand::default()
        .with_mode(DetectionMode::Putting)
        .with_club(Club::Putter);
    assert_eq!(
        describe(&ServerEvent::Command(&cmd), Level::Info).as_deref(),
        Some("command: mode=putting hand=- club=PT")
    );
}
