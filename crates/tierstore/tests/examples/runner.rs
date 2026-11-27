//! The runner over small examples of the tests' own. Their client is this crate's `tierstore`
//! binary: `--version` prints its version and exits 0, an unknown flag exits 2. Their tierstore
//! is a node with a write-through cache over a `dir` cold tier, as the compiler-cache example's.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;

use tierstore_examples::Error;
use tierstore_examples::report::Report;
use tierstore_examples::run::{Options, run};
use tierstore_testkit::dir::TestDir;

use crate::{scratch, tierstore};

/// The configuration of the stand-in examples.
const CONFIG: &str = r#"[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:0"] }

[server]
proxy_id        = "runner-test"
bucket          = "runner"
access_key_file = "${work}/access.key"
drain_lb_wait   = "1s"

[local_cache]
path          = "${work}/cache"
budget        = "64MiB"
write_through = true

[cold_store]
kind = "dir"
path = "${work}/cold"

[[policy]]
prefix = "kept/"
cache  = true
"#;

/// The template of the stand-ins' JSON files.
const TEMPLATE: &str = "{\"n\": ${n}, \"text\": \"t${n}\", \"flag\": true}\n";

/// The template of a file that is not JSON.
const NOT_JSON: &str = "not json ${n}\n";

/// The template of `other/1.json`, which differs from `more/1.json`.
const OTHER: &str = "{}\n";

/// Generators, an environment with a free port and the endpoint, and one check of each kind.
const CHECKS: &str = r#"
[[generate]]
template = "data.json.in"
path     = "data/${n}.json"
count    = 2

[[generate]]
template = "data.json.in"
path     = "more/${n}.json"
count    = 1

[[generate]]
template = "other.json.in"
path     = "other/${n}.json"
count    = 1

[env]
BUCKET = "${bucket}"
SPARE  = "${port:spare}"
FRONT  = "${endpoint}"

[[check]]
name = "json.n"
json = { file = "data/1.json", pointer = "/n" }

[[check]]
name = "json.text"
json = { file = "data/2.json", pointer = "/text" }

[[check]]
name = "json.default"
json = { file = "data/1.json", pointer = "/none", default = "0" }

[[check]]
name = "json.flag"
json = { file = "data/1.json", pointer = "/flag" }

[[check]]
name = "same.dirs"
same = { a = "data", b = "data" }

[[check]]
name = "same.files"
same = { a = "data/1.json", b = "data/2.json" }

[[check]]
name = "same.only_a"
same = { a = "data", b = "more" }

[[check]]
name = "same.only_b"
same = { a = "more", b = "data" }

[[check]]
name = "same.inside"
same = { a = "more", b = "other" }

[[check]]
name = "metric.hits"
metric = { series = "tierstore_cache_hits_total", step = "one" }

[[check]]
name = "metric.none"
metric = { series = "no_such_series", step = "one" }

[[check]]
name = "objects"
objects = { prefix = "" }
"#;

/// The lines of [`CHECKS`] over a run that wrote nothing to the store.
const LINES: &str = "json.n 1\njson.text t2\njson.default 0\njson.flag true\nsame.dirs same\n\
same.files differ at data/1.json\nsame.only_a only data/2.json\nsame.only_b only data/2.json\n\
same.inside differ at more/1.json\nmetric.hits 0\nmetric.none absent\nobjects 0\n";

/// The last line of every report.
const STOPPED: &str = "tierstore.stop exit status: 0\n";

/// A check that is always there to be evaluated.
const ONE_CHECK: &str = "[[check]]\nname = \"c\"\nobjects = { prefix = \"\" }\n";

/// The first line `tierstore --version` prints.
fn version() -> String {
    let out = Command::new(tierstore()).arg("--version").output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    text.lines().next().unwrap().trim().to_owned()
}

/// The binary, as a TOML string.
fn bin() -> String {
    format!("{:?}", tierstore())
}

fn path(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// A page that shows each of `files` as it is.
fn page_of(files: &[(&str, &str)]) -> String {
    let blocks: Vec<String> = files
        .iter()
        .map(|(name, text)| format!("```text file={name}\n{text}```\n"))
        .collect();
    format!("# A stand-in\n\n{}", blocks.join("\n"))
}

/// A stand-in example in `dir`: `example.toml` (`spec`), `tierstore.toml` (`config`),
/// `expected.txt` (`expected`), the templates, and a page that shows them all. Gives the options
/// of a run of it with an empty work directory.
fn standin(dir: &TestDir, spec: &str, config: &str, expected: &str) -> Options {
    let example = dir.path().join("example");
    std::fs::create_dir_all(&example).unwrap();
    let files = [
        ("example.toml", spec),
        ("tierstore.toml", config),
        ("expected.txt", expected),
        ("data.json.in", TEMPLATE),
        ("bad.in", NOT_JSON),
        ("other.json.in", OTHER),
    ];
    for (name, text) in &files {
        std::fs::write(example.join(name), text).unwrap();
    }
    std::fs::write(example.join("page.md"), page_of(&files)).unwrap();
    Options {
        example,
        tierstore: tierstore(),
        work: dir.path().join("work"),
        tmp: None,
        config: None,
    }
}

/// A spec of the client at its own version with one step `one` that runs `argv` (`tierstore` is
/// the binary), then `rest`.
fn spec(argv: &[&str], rest: &str) -> String {
    let words: Vec<String> = argv
        .iter()
        .map(|word| match *word {
            "tierstore" => bin(),
            other => format!("{other:?}"),
        })
        .collect();
    format!(
        "[client]\nprogram = {}\nversion_args = [\"--version\"]\nversion = {:?}\n\n\
         [[step]]\nname = \"one\"\ncommands = [{{ argv = [{}] }}]\n{rest}",
        bin(),
        version(),
        words.join(", ")
    )
}

/// The spec of a run that gives [`LINES`], with `rest` after its checks.
fn checks_spec(rest: &str) -> String {
    spec(&["tierstore", "--version"], &format!("{CHECKS}{rest}"))
}

fn run_ok(options: &Options) -> Report {
    run(options).unwrap_or_else(|e| panic!("{e}"))
}

/// The error of a run that ends early.
fn run_err(options: &Options) -> Error {
    match run(options) {
        Ok(report) => panic!("the run passed: {report:?}"),
        Err(error) => error,
    }
}

/// Asserts a run whose lines are its expected.txt passes, with one line for each kind of check
/// (a JSON string, number, `true` and default; trees and files the same, differing, and with a
/// file in one only; a metric's rise and an absent series; the objects listed) and tierstore's
/// exit status last; that a command's standard output goes to the file it names, and the closing
/// commands run after the steps.
#[test]
fn a_run_gives_one_line_for_each_check_and_passes_when_they_are_expected() {
    let dir = scratch("runner-all-kinds");
    let cleanup = format!(
        "\n[[cleanup]]\nargv = [{}, \"--version\"]\nstdout = \"cleaned.txt\"\n",
        bin()
    );
    let text =
        checks_spec(&cleanup).replacen("{ argv = [", "{ stdout = \"version.txt\", argv = [", 1);
    let expected = format!("{LINES}{STOPPED}");
    let options = standin(&dir, &text, CONFIG, &expected);
    let report = run_ok(&options);
    assert_eq!(report.actual, expected);
    assert!(report.passed(), "{}", report.describe());
    assert_eq!(report.describe(), "");
    let work = &options.work;
    let printed = std::fs::read_to_string(work.join("version.txt")).unwrap();
    assert_eq!(printed.lines().next().unwrap(), version());
    let cleaned = std::fs::read_to_string(work.join("cleaned.txt")).unwrap();
    assert_eq!(cleaned, printed);
    let log = std::fs::read_to_string(work.join("logs/tierstore.log")).unwrap();
    assert!(log.contains(" listen=127.0.0.1:"), "{log}");
}

/// Asserts a report that differs from expected.txt fails, and names the lines that differ: a
/// value that changed, a line given and not expected, and a line expected and not given.
#[test]
fn a_report_that_differs_names_each_line_that_differs() {
    let dir = scratch("runner-differs");
    let expected = LINES
        .replace("objects 0\n", "objects 1\n")
        .replace("metric.none absent\n", "")
        + "gone 1\n"
        + STOPPED;
    let options = standin(&dir, &checks_spec(""), CONFIG, &expected);
    let report = run_ok(&options);
    assert!(!report.passed());
    assert_eq!(report.differing(), ["metric.none", "objects", "gone"]);
    assert_eq!(
        report.describe(),
        "- expected: (no line)\n+ found:    metric.none absent\n\
         - expected: objects 1\n+ found:    objects 0\n\
         - expected: gone 1\n+ found:    (no line)\n"
    );
}

/// Asserts a check with `expected_failure` gives its line with the issue; that the run passes
/// while its value is not the one it has once the feature is there, and fails, saying why, once
/// it is.
#[test]
fn an_expected_failure_is_marked_and_fails_the_run_once_it_passes() {
    let xfail = |passing: &str| {
        format!(
            "\n[[check]]\nname = \"xfail\"\nmetric = {{ series = \"tierstore_cache_hits_total\", step = \"one\" }}\n\
             expected_failure = {{ issue = \"komira-ai/tierstore#1\", passing = \"{passing}\" }}\n"
        )
    };
    let expected = format!("{LINES}xfail 0 (expected failure: komira-ai/tierstore#1)\n{STOPPED}");

    let dir = scratch("runner-xfail-failing");
    let report = run_ok(&standin(&dir, &checks_spec(&xfail("1")), CONFIG, &expected));
    assert!(report.passed(), "{}", report.describe());

    let dir = scratch("runner-xfail-passing");
    let report = run_ok(&standin(&dir, &checks_spec(&xfail("0")), CONFIG, &expected));
    let why = "xfail is 0, which it is once komira-ai/tierstore#1 is done: make it a check like the others";
    assert_eq!(report.actual, expected);
    assert_eq!(report.now_passing, [why]);
    assert!(!report.passed());
    assert_eq!(report.describe(), format!("! {why}\n"));
}

/// Asserts a command that exits with another status than 0 ends the run with the step, the
/// command and the status, and that the closing commands still run, each whatever the one before
/// did: a closing command that fails is written to their log.
#[test]
fn a_failed_command_ends_the_run_and_the_closing_commands_still_run() {
    let dir = scratch("runner-command-fails");
    let bin = bin();
    let cleanup = format!(
        "\n[[cleanup]]\nargv = [{bin}, \"--no-such-flag\"]\n\n[[cleanup]]\nargv = [{bin}, \"--version\"]\nstdout = \"cleaned.txt\"\n\n{ONE_CHECK}"
    );
    let options = standin(
        &dir,
        &spec(&["tierstore", "--no-such-flag"], &cleanup),
        CONFIG,
        "",
    );
    let error = run_err(&options);
    let work = std::fs::canonicalize(&options.work).unwrap();
    let command = format!("{} --no-such-flag", tierstore().display());
    let one = work.join("logs/one.log");
    assert_eq!(
        error.to_string(),
        format!(
            "step one: `{command}` ended with exit status: 2; its output is in {}",
            one.display()
        )
    );
    assert!(work.join("cleaned.txt").is_file());
    let closing_log = work.join("logs/cleanup.log");
    let closing = std::fs::read_to_string(&closing_log).unwrap();
    let said = format!(
        "step cleanup: `{command}` ended with exit status: 2; its output is in {}\n",
        closing_log.display()
    );
    assert!(closing.ends_with(&said), "{closing}");
}

/// Asserts each other way a run ends early is told apart, with the file or program it is about:
/// a step's program that is not there, a client that is not installed or is of another version,
/// a tierstore that refuses its file, and a work directory that is not empty.
#[test]
fn a_run_that_cannot_start_its_programs_says_which() {
    let dir = scratch("runner-spawn");
    let options = standin(&dir, &spec(&["/no/such/program"], ONE_CHECK), CONFIG, "");
    let error = run_err(&options);
    assert!(
        error
            .to_string()
            .starts_with("step one: /no/such/program could not be started: "),
        "{error}"
    );

    let dir = scratch("runner-client-missing");
    let text =
        spec(&["tierstore", "--version"], ONE_CHECK).replacen(&bin(), "\"/no/such/client\"", 1);
    let error = run_err(&standin(&dir, &text, CONFIG, ""));
    let Error::ClientMissing { program, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(program, "/no/such/client");

    let dir = scratch("runner-client-version");
    let text =
        spec(&["tierstore", "--version"], ONE_CHECK).replacen(&version(), "tierstore 0.0.0", 1);
    let error = run_err(&standin(&dir, &text, CONFIG, ""));
    assert_eq!(
        error.to_string(),
        format!(
            "the client {} is {:?}, not \"tierstore 0.0.0\"",
            tierstore().display(),
            version()
        )
    );

    let dir = scratch("runner-refused-config");
    let refused = CONFIG.replace("budget        = \"64MiB\"", "budget        = \"lots\"");
    let error = run_err(&standin(
        &dir,
        &spec(&["tierstore", "--version"], ONE_CHECK),
        &refused,
        "",
    ));
    let Error::TierstoreStart {
        status, log_text, ..
    } = &error
    else {
        panic!("{error}");
    };
    assert_eq!(status, "exit status: 2");
    assert!(log_text.contains("budget"), "{log_text}");

    let dir = scratch("runner-not-empty");
    let options = standin(
        &dir,
        &spec(&["tierstore", "--version"], ONE_CHECK),
        CONFIG,
        "",
    );
    std::fs::create_dir_all(&options.work).unwrap();
    std::fs::write(options.work.join("left"), "").unwrap();
    assert_eq!(
        run_err(&options).to_string(),
        format!(
            "{}: the work directory is not empty",
            options.work.display()
        )
    );
}

/// Asserts a spec, an environment, an argument or a configuration that names a value the run
/// does not have, a configuration with no bucket, and a spec with a key the runner does not know,
/// each end the run naming the file.
#[test]
fn a_run_over_a_file_it_cannot_read_names_the_file() {
    let ok = spec(&["tierstore", "--version"], ONE_CHECK);
    let cases = [
        (
            "env",
            ok.replacen("[[step]]", "[env]\nX = \"${nope}\"\n\n[[step]]", 1),
            CONFIG.to_owned(),
            "example.toml",
            "${nope} is not a value of the run",
        ),
        (
            "step-env",
            ok.replacen(
                "name = \"one\"\n",
                "name = \"one\"\nenv = { X = \"${nope}\" }\n",
                1,
            ),
            CONFIG.to_owned(),
            "example.toml",
            "${nope} is not a value of the run",
        ),
        (
            "arg",
            ok.replacen("\"--version\"] }", "\"${nope\"] }", 1),
            CONFIG.to_owned(),
            "example.toml",
            "a ${ has no } after it",
        ),
        (
            "endpoint-in-config",
            ok.clone(),
            CONFIG.replace("runner-test", "${endpoint}"),
            "tierstore.toml",
            "${endpoint} is not a value of the run",
        ),
        (
            "no-bucket",
            ok.clone(),
            CONFIG.replace("bucket          = \"runner\"\n", ""),
            "tierstore.toml",
            "it has no server.bucket",
        ),
        (
            "spec-key",
            format!("surprise = 1\n{ok}"),
            CONFIG.to_owned(),
            "example.toml",
            "unknown field `surprise`, expected one of `client`, `generate`, `env`, `step`, `cleanup`, `check`",
        ),
    ];
    for (name, text, config, file, why) in cases {
        let dir = scratch(&format!("runner-file-{name}"));
        let options = standin(&dir, &text, &config, "");
        let example = std::fs::canonicalize(&options.example).unwrap();
        let expected = format!("{}: {why}", example.join(file).display());
        assert_eq!(run_err(&options).to_string(), expected, "{name}");
    }
}

/// Asserts a check that cannot be evaluated ends the run naming it: a JSON file that is not
/// there or is not JSON, a pointer at a table, a pointer that finds nothing with no default, and
/// a `same` of a path that is not there.
#[test]
fn a_check_that_cannot_be_evaluated_is_named_with_its_reason() {
    let generate = "\n[[generate]]\ntemplate = \"data.json.in\"\npath = \"data/${n}.json\"\ncount = 1\n\n\
                    [[generate]]\ntemplate = \"bad.in\"\npath = \"bad/${n}.json\"\ncount = 1\n";
    let cases = [
        (
            "missing",
            "json = { file = \"none.json\", pointer = \"/n\" }",
            "<work>/none.json: No such file or directory (os error 2)",
        ),
        (
            "not-json",
            "json = { file = \"bad/1.json\", pointer = \"/n\" }",
            "<work>/bad/1.json: expected ident at line 1 column 2",
        ),
        (
            "table",
            "json = { file = \"data/1.json\", pointer = \"\" }",
            "data/1.json holds a list or a table at , not one value",
        ),
        (
            "nothing",
            "json = { file = \"data/1.json\", pointer = \"/none\" }",
            "data/1.json holds nothing at /none",
        ),
        (
            "same",
            "same = { a = \"data\", b = \"none\" }",
            "<work>/none: No such file or directory (os error 2)",
        ),
    ];
    for (name, kind, why) in cases {
        let dir = scratch(&format!("runner-check-{name}"));
        let rest = format!("{generate}\n[[check]]\nname = \"c\"\n{kind}\n");
        let options = standin(&dir, &spec(&["tierstore", "--version"], &rest), CONFIG, "");
        let error = run_err(&options);
        let work = std::fs::canonicalize(&options.work).unwrap();
        let why = why.replace("<work>", path(&work));
        assert_eq!(error.to_string(), format!("check c: {why}"), "{name}");
    }
}

/// Asserts what the generators refuse: a template that is not there, a path naming a value other
/// than `${n}`, and two generators that write one file.
#[test]
fn a_generator_that_cannot_write_its_files_says_why() {
    let generator = |template: &str, path: &str| {
        format!("\n[[generate]]\ntemplate = \"{template}\"\npath = \"{path}\"\ncount = 1\n")
    };
    let cases = [
        (
            "missing",
            generator("none.in", "x/${n}"),
            "<example>/none.in: No such file or directory (os error 2)",
        ),
        (
            "value",
            generator("data.json.in", "${work}/${n}"),
            "<example>/example.toml: ${work} is not a value of the run",
        ),
        (
            "twice",
            generator("data.json.in", "x/${n}") + &generator("data.json.in", "x/${n}"),
            "<work>/x/1: File exists (os error 17)",
        ),
    ];
    for (name, generators, why) in cases {
        let dir = scratch(&format!("runner-generate-{name}"));
        let rest = format!("{generators}\n{ONE_CHECK}");
        let options = standin(&dir, &spec(&["tierstore", "--version"], &rest), CONFIG, "");
        let error = run_err(&options);
        let example = std::fs::canonicalize(&options.example).unwrap();
        let work = std::fs::canonicalize(&options.work).unwrap();
        let why = why
            .replace("{example}", path(&example))
            .replace("{work}", path(&work));
        assert_eq!(error.to_string(), why, "{name}");
    }
}

/// The words of `args`, borrowed.
fn words(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

/// Runs the command line `args` and gives its status, standard output and standard error.
fn cli(args: &[&str]) -> (u8, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let args: Vec<OsString> = args.iter().map(Into::into).collect();
    let code = tierstore_examples::cli::main(args, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// Asserts the command line: `run` prints the report and exits 0 when it passes; prints what
/// differs and exits 1 when it does not; exits 1 with the error when the run cannot finish, and
/// with `--tierstore-config` runs the configuration it names; `--help` prints on standard output
/// and exits 0; a missing verb exits 2 with the usage on standard error.
#[test]
fn the_command_line_runs_an_example_and_exits_with_its_verdict() {
    let bin = tierstore();
    let expected = format!("{LINES}{STOPPED}");
    let run_args = |options: &Options| {
        vec![
            "tierstore-example".to_owned(),
            "run".to_owned(),
            path(&options.example).to_owned(),
            "--tierstore".to_owned(),
            path(&bin).to_owned(),
            "--work".to_owned(),
            path(&options.work).to_owned(),
        ]
    };

    let dir = scratch("cli-pass");
    let options = standin(&dir, &checks_spec(""), CONFIG, &expected);
    assert_eq!(
        cli(&words(&run_args(&options))),
        (0, expected.clone(), String::new())
    );

    let dir = scratch("cli-differs");
    let options = standin(&dir, &checks_spec(""), CONFIG, "");
    let (code, out, err) = cli(&words(&run_args(&options)));
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(
        err.starts_with("- expected: (no line)\n+ found:    json.n 1\n"),
        "{err}"
    );

    let dir = scratch("cli-config");
    let options = standin(&dir, &checks_spec(""), CONFIG, &expected);
    let refused = dir.path().join("refused.toml");
    std::fs::write(
        &refused,
        CONFIG.replace("bucket          = \"runner\"\n", ""),
    )
    .unwrap();
    let mut args = run_args(&options);
    args.extend(["--tierstore-config".to_owned(), path(&refused).to_owned()]);
    let (code, out, err) = cli(&words(&args));
    assert_eq!((code, out.as_str()), (1, ""));
    assert_eq!(
        err,
        format!(
            "tierstore-example: {}: it has no server.bucket\n",
            refused.display()
        )
    );

    let (code, out, err) = cli(&["tierstore-example", "--help"]);
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(out.contains("check-page"), "{out}");
    let (code, out, err) = cli(&["tierstore-example"]);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("Usage: tierstore-example"), "{err}");
}

/// Asserts `count_objects` reads every page of a listing: three objects put through tierstore are
/// counted in pages of one and in one page, under a prefix and over the whole bucket; and that a
/// listing of a front that is gone is an error.
#[test]
fn the_objects_of_a_prefix_are_counted_over_every_page() {
    use objstore_s3::s3::{Addressing, Credentials, S3Config, S3Store};
    use objstore_s3::{Capabilities, ObjectKey, ObjectStore as _, PageSize};
    use tierstore_examples::run::count_objects;
    use tierstore_examples::server::Tierstore;
    use tierstore_examples::{ACCESS_KEY_ID, SECRET_ACCESS_KEY};

    let dir = scratch("runner-count");
    let work = dir.path();
    let config = work.join("tierstore.toml");
    std::fs::write(&config, CONFIG.replace("${work}", path(work))).unwrap();
    let key = work.join("access.key");
    std::fs::write(&key, format!("{ACCESS_KEY_ID}\n{SECRET_ACCESS_KEY}\n")).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let log = work.join("tierstore.log");
    let server = Tierstore::start(&tierstore(), &config, &log).unwrap();
    assert_eq!(server.log(), log);
    let addr = server.addr();
    let store = S3Store::new(S3Config {
        endpoint: format!("http://{addr}"),
        region: "us-east-1".to_owned(),
        bucket: "runner".to_owned(),
        addressing: Addressing::Auto,
        ca_pem: None,
        credentials: Credentials::new(ACCESS_KEY_ID, SECRET_ACCESS_KEY),
        capabilities: Capabilities::default(),
        connect_timeout: std::time::Duration::from_secs(10),
        request_timeout: std::time::Duration::from_secs(60),
        min_body_rate: None,
    })
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for name in ["kept/a", "kept/b", "other/c"] {
        let key = ObjectKey::new(name).unwrap();
        runtime
            .block_on(store.put_new(&key, bytes::Bytes::from_static(b"x"), None))
            .unwrap();
    }
    drop(runtime);
    let one = PageSize::new(1).unwrap();
    assert_eq!(count_objects(addr, "runner", "kept/", one), Ok(2));
    assert_eq!(count_objects(addr, "runner", "", one), Ok(3));
    assert_eq!(count_objects(addr, "runner", "", PageSize::MAX), Ok(3));
    assert_eq!(server.stop().unwrap(), "exit status: 0");
    assert!(count_objects(addr, "runner", "", one).is_err());
}
