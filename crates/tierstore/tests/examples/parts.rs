//! The parts of the runner as functions: the placeholders, what a spec refuses, the metrics page
//! and a reading of it, and tierstore's start line.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;

use tierstore_examples::metrics::{Reading, parse, rise, scrape};
use tierstore_examples::server::listen_address;
use tierstore_examples::spec::{Example, expand, port_names};

fn vars() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("work".to_owned(), "/w".to_owned()),
        ("port:a".to_owned(), "1".to_owned()),
    ])
}

/// Asserts a placeholder is its value, `$$` is one `$`, a `$` before anything else is itself, and
/// an unknown name or a `${` with no `}` is refused with what is wrong.
#[test]
fn a_placeholder_is_its_value_and_an_unknown_one_is_refused() {
    let vars = vars();
    assert_eq!(
        expand("${work}/x:${port:a}", &vars),
        Ok("/w/x:1".to_owned())
    );
    assert_eq!(expand("$$ $(CC) a$", &vars), Ok("$ $(CC) a$".to_owned()));
    assert_eq!(
        expand("${nope}", &vars),
        Err("${nope} is not a value of the run".to_owned())
    );
    assert_eq!(
        expand("x ${work", &vars),
        Err("a ${ has no } after it".to_owned())
    );
    let names = port_names(&["${port:b} ${port:a}", "${port:a}", "${port:open"]);
    assert_eq!(names.into_iter().collect::<Vec<_>>(), ["a", "b"]);
}

/// The smallest spec that parses, with `step` and `check` as its last tables.
fn spec(step: &str, check: &str) -> String {
    format!(
        "[client]\nprogram = \"c\"\nversion_args = []\nversion = \"c 1\"\n\n[[step]]\n{step}\n\n[[check]]\n{check}\n"
    )
}

const STEP: &str = "name = \"one\"\ncommands = [{ argv = [\"c\"], stdout = \"o.txt\" }]";
const CHECK: &str = "name = \"c\"\nobjects = { prefix = \"\" }";

/// Asserts the spec refuses each malformed table with what is wrong: step names that are empty,
/// badly formed or used twice; a step with no command and a command with no program; paths that
/// are absolute or leave the directory; a stdout named twice; a generator with no `${n}` or a
/// count of 0; check names that are not one word or are used twice; a check of no kind or two;
/// and a metric of a step that does not exist.
#[test]
fn a_spec_refuses_each_malformed_table() {
    let good = spec(STEP, CHECK);
    assert!(Example::parse(&good).is_ok());
    let two_steps = |second: &str| format!("{STEP}\n\n[[step]]\n{second}");
    let cases = [
        (
            spec("name = \"\"\ncommands = [{ argv = [\"c\"] }]", CHECK),
            "step \"\": a name is lowercase letters, digits and -, and is not used twice",
        ),
        (
            spec("name = \"One\"\ncommands = [{ argv = [\"c\"] }]", CHECK),
            "step \"One\": a name is lowercase letters, digits and -, and is not used twice",
        ),
        (
            spec(
                &two_steps("name = \"one\"\ncommands = [{ argv = [\"c\"] }]"),
                CHECK,
            ),
            "step \"one\": a name is lowercase letters, digits and -, and is not used twice",
        ),
        (
            spec("name = \"one\"\ncommands = []", CHECK),
            "step one: it has no command",
        ),
        (
            spec("name = \"one\"\ncommands = [{ argv = [] }]", CHECK),
            "a command has no program",
        ),
        (
            spec(
                "name = \"one\"\ncommands = [{ argv = [\"c\"], stdout = \"/o\" }]",
                CHECK,
            ),
            "\"/o\" is not a relative path inside the directory",
        ),
        (
            spec(
                "name = \"one\"\ncommands = [{ argv = [\"c\"], stdout = \"a/../o\" }]",
                CHECK,
            ),
            "\"a/../o\" is not a relative path inside the directory",
        ),
        (
            spec(
                "name = \"one\"\ncommands = [{ argv = [\"c\"], stdout = \"\" }]",
                CHECK,
            ),
            "\"\" is not a relative path inside the directory",
        ),
        (
            spec(
                &two_steps("name = \"two\"\ncommands = [{ argv = [\"c\"], stdout = \"o.txt\" }]"),
                CHECK,
            ),
            "o.txt is the stdout of two commands",
        ),
        (
            spec(STEP, "name = \"a b\"\nobjects = { prefix = \"\" }"),
            "check \"a b\": a name is one word",
        ),
        (
            spec(STEP, "name = \"\"\nobjects = { prefix = \"\" }"),
            "check \"\": a name is one word",
        ),
        (
            spec(STEP, &format!("{CHECK}\n\n[[check]]\n{CHECK}")),
            "check c: the name is used twice",
        ),
        (
            spec(STEP, "name = \"c\""),
            "check c: give exactly one of json, metric, same and objects",
        ),
        (
            spec(
                STEP,
                "name = \"c\"\nobjects = { prefix = \"\" }\nsame = { a = \"x\", b = \"y\" }",
            ),
            "check c: give exactly one of json, metric, same and objects",
        ),
        (
            spec(
                STEP,
                "name = \"c\"\nmetric = { series = \"s\", step = \"two\" }",
            ),
            "check c: there is no step two",
        ),
        (
            spec(
                STEP,
                "name = \"c\"\njson = { file = \"../x\", pointer = \"\" }",
            ),
            "\"../x\" is not a relative path inside the directory",
        ),
        (
            spec(STEP, "name = \"c\"\nsame = { a = \"x\", b = \"/y\" }"),
            "\"/y\" is not a relative path inside the directory",
        ),
        (
            spec(STEP, "name = \"c\"\nsame = { a = \"/x\", b = \"y\" }"),
            "\"/x\" is not a relative path inside the directory",
        ),
        (
            format!("[[generate]]\ntemplate = \"t\"\npath = \"x\"\ncount = 1\n\n{good}"),
            "generate x: the path needs ${n} and the count at least 1",
        ),
        (
            format!("[[generate]]\ntemplate = \"t\"\npath = \"x${{n}}\"\ncount = 0\n\n{good}"),
            "generate x${n}: the path needs ${n} and the count at least 1",
        ),
        (
            format!("[[generate]]\ntemplate = \"/t\"\npath = \"x${{n}}\"\ncount = 1\n\n{good}"),
            "\"/t\" is not a relative path inside the directory",
        ),
        (
            format!("[[generate]]\ntemplate = \"t\"\npath = \"/x${{n}}\"\ncount = 1\n\n{good}"),
            "\"/x${n}\" is not a relative path inside the directory",
        ),
    ];
    for (text, why) in cases {
        assert_eq!(Example::parse(&text), Err(why.to_owned()), "{text}");
    }
    let cleanup = format!("[[cleanup]]\nargv = [\"c\"]\nstdout = \"o.txt\"\n\n{good}");
    assert_eq!(
        Example::parse(&cleanup),
        Err("o.txt is the stdout of two commands".to_owned())
    );
}

/// Asserts the reading of a metrics page: comments and blank lines skipped; labels with spaces,
/// quotes, escapes and braces in their values, and spaces between labels, kept whole as the
/// series; a quote or backslash outside labels taken as part of the name; a timestamp dropped;
/// and a line with no value left out.
#[test]
fn a_metrics_page_is_read_by_series() {
    let page = "# TYPE a counter\n\na 1\nb{op=\"get\"} 2 1700000000\nc{l=\"x y\\\" }{\"} 3\nd{l=\"v\"}\ne\t4\n\
                q\"x 5\nr\\y 6\ns{a=\"1\", b=\"2\"} 7\n";
    let reading = parse(page);
    let expected: Reading = [
        ("a", "1"),
        ("b{op=\"get\"}", "2"),
        ("c{l=\"x y\\\" }{\"}", "3"),
        ("e", "4"),
        ("q\"x", "5"),
        ("r\\y", "6"),
        ("s{a=\"1\", b=\"2\"}", "7"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    assert_eq!(reading, expected);
}

/// Asserts how a series' rise reads: a whole number (negative too), a fraction, a number too
/// large to be an exact integer, `absent` in either reading or both, and `not a number`.
#[test]
fn a_rise_is_a_whole_number_or_says_why_it_is_not() {
    let before = parse("a 3\nb 1.5\nc 2\nd x\nf 1e3\nh 0\n");
    let after = parse("a 5\nb 2\nc 1\nd 1\ne 1\nf 1e3\nh 1e20\n");
    let rises: Vec<String> = ["a", "b", "c", "d", "e", "f", "g", "h"]
        .iter()
        .map(|s| rise(&before, &after, s))
        .collect();
    assert_eq!(
        rises,
        [
            "2",
            "0.5",
            "-1",
            "not a number",
            "absent before",
            "0",
            "absent",
            "100000000000000000000"
        ]
    );
    assert_eq!(rise(&after, &before, "e"), "absent after");
}

/// Asserts the start line gives its first address, and that other lines and a start line with no
/// address give none.
#[test]
fn the_start_line_gives_the_first_address() {
    let line = "tierstore 0.1.0+abc listen=127.0.0.1:9000,127.0.0.1:9001 mode=write-through-cache commit=abc";
    assert_eq!(
        listen_address(line),
        Some("127.0.0.1:9000".parse().unwrap())
    );
    assert_eq!(listen_address("INFO listen=127.0.0.1:1"), None);
    assert_eq!(listen_address("tierstore 0.1.0 mode=x"), None);
    assert_eq!(listen_address("tierstore 0.1.0 listen=nowhere"), None);
}

/// Asserts a read of `/metricz` that is not answered 200, or whose server is gone, is an error
/// naming the address.
#[test]
fn a_metrics_page_that_cannot_be_read_is_an_error() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let answering = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        stream
            .write_all(b"HTTP/1.0 404 Not Found\r\n\r\nno")
            .unwrap();
    });
    let error = scrape(addr).err().unwrap();
    answering.join().unwrap();
    assert_eq!(
        error.to_string(),
        format!("reading /metricz of {addr}: the answer is \"HTTP/1.0 404 Not Found\"")
    );
    let gone = tierstore_testkit::port::dead_addr();
    let error = scrape(gone).err().unwrap();
    assert!(
        error
            .to_string()
            .starts_with(&format!("reading /metricz of {gone}: ")),
        "{error}"
    );
}
