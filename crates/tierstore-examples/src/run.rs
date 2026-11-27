//! One run of an example, from an empty work directory to its [`Report`].

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use objstore_s3::s3::{Addressing, Credentials, S3Config, S3Store};
use objstore_s3::{Capabilities, KeyPrefix, ListToken, ObjectStore as _, PageSize};

use crate::checks::{Marks, evaluate};
use crate::error::{Error, invalid_at, io_at};
use crate::generate::generate;
use crate::metrics::scrape;
use crate::page;
use crate::report::Report;
use crate::server::Tierstore;
use crate::spec::{CONFIG_FILE, EXPECTED_FILE, Example, SPEC_FILE, expand, port_names};
use crate::steps::Runner;
use crate::{ACCESS_KEY_ID, SECRET_ACCESS_KEY};

/// What a run is given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The example's directory.
    pub example: PathBuf,
    /// The `tierstore` binary.
    pub tierstore: PathBuf,
    /// The work directory: made if it is not there, and refused if it holds anything.
    pub work: PathBuf,
    /// The `TMPDIR` of the commands, made if it is not there; `None` for `${work}/tmp`. A client
    /// that binds a Unix socket there (sccache does, at its start) needs a path short enough for
    /// one, about 100 bytes, which a work directory deep in a build tree can pass.
    pub tmp: Option<PathBuf>,
    /// A tierstore configuration to run with in place of the example's `tierstore.toml` (the
    /// same placeholders): to see the checks fail over a tierstore set up another way.
    pub config: Option<PathBuf>,
}

/// Runs the example: checks its page, starts tierstore, checks the client, generates the inputs,
/// runs the steps with a reading of `/metricz` before and after each, runs the closing commands,
/// evaluates the checks and stops tierstore. The report's last line is tierstore's exit status
/// after the stop, `tierstore.stop exit status: 0`.
///
/// # Errors
/// The page differs from the files; a file of the example cannot be read or is invalid; the work
/// directory is not empty; tierstore does not start; the client is missing or of another
/// version; a command fails; `/metricz` cannot be read; a check cannot be evaluated.
pub fn run(options: &Options) -> Result<Report, Error> {
    let example = fs::canonicalize(&options.example).map_err(io_at(&options.example))?;
    page::check(&example)?;
    let spec_path = example.join(SPEC_FILE);
    let spec = Example::parse(&read(&spec_path)?).map_err(invalid_at(&spec_path))?;
    let expected = read(&example.join(EXPECTED_FILE))?;
    let work = prepare(&options.work)?;
    let mut vars = BTreeMap::from([
        ("work".to_owned(), work.display().to_string()),
        ("example".to_owned(), example.display().to_string()),
        ("access_key_id".to_owned(), ACCESS_KEY_ID.to_owned()),
        ("secret_access_key".to_owned(), SECRET_ACCESS_KEY.to_owned()),
    ]);
    // Every port is held until all are picked, so no two names get one port.
    let mut held = Vec::new();
    for name in port_names(&spec.expanded_texts()) {
        let (port, listener) = free_port(&work)?;
        vars.insert(format!("port:{name}"), port.to_string());
        held.push(listener);
    }
    drop(held);
    let config_path = options
        .config
        .clone()
        .unwrap_or_else(|| example.join(CONFIG_FILE));
    let config = expand(&read(&config_path)?, &vars).map_err(invalid_at(&config_path))?;
    let bucket = bucket_of(&config).map_err(invalid_at(&config_path))?;
    vars.insert("bucket".to_owned(), bucket.clone());
    write_private(
        &work.join("access.key"),
        &format!("{ACCESS_KEY_ID}\n{SECRET_ACCESS_KEY}\n"),
    )?;
    let config_file = work.join(CONFIG_FILE);
    write_private(&config_file, &config)?;

    let log = work.join("logs").join("tierstore.log");
    let server = Tierstore::start(&options.tierstore, &config_file, &log)?;
    let addr = server.addr();
    vars.insert("endpoint".to_owned(), format!("http://{addr}"));
    let tmp = options.tmp.clone().unwrap_or_else(|| work.join("tmp"));
    fs::create_dir_all(&tmp).map_err(io_at(&tmp))?;
    let runner = Runner::new(&work, &tmp, &example, &vars, &spec.env)?;
    runner.check_client(&spec.client)?;
    generate(&example, &work, &spec.generate)?;
    let steps = run_steps(&runner, &spec, addr);
    runner.cleanup(&spec.cleanup);
    let marks = steps?;
    let page = PageSize::MAX;
    let mut objects = |prefix: &str| count_objects(addr, &bucket, prefix, page);
    let evaluated = evaluate(&spec.check, &work, &marks, &mut objects)?;
    let status = server.stop()?;
    let actual = format!("{}tierstore.stop {status}\n", evaluated.lines);
    Ok(Report {
        actual,
        expected,
        now_passing: evaluated.now_passing,
    })
}

/// Runs each step between two readings of `/metricz`.
fn run_steps(runner: &Runner<'_>, spec: &Example, addr: SocketAddr) -> Result<Marks, Error> {
    let mut marks = Marks::new();
    for step in &spec.step {
        let env = runner.with(&step.env)?;
        let before = scrape(addr)?;
        for command in &step.commands {
            runner.run(&step.name, command, &env)?;
        }
        let after = scrape(addr)?;
        marks.insert(step.name.clone(), (before, after));
    }
    Ok(marks)
}

/// How many objects the bucket `bucket` of the S3 front at `addr` lists under `prefix`, read in
/// pages of `page`.
///
/// # Errors
/// The prefix is not one S3 takes, or a page cannot be listed.
pub fn count_objects(
    addr: SocketAddr,
    bucket: &str,
    prefix: &str,
    page: PageSize,
) -> Result<u64, String> {
    let config = S3Config {
        endpoint: format!("http://{addr}"),
        region: "us-east-1".to_owned(),
        bucket: bucket.to_owned(),
        addressing: Addressing::Auto,
        ca_pem: None,
        credentials: Credentials::new(ACCESS_KEY_ID, SECRET_ACCESS_KEY),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(10),
        request_timeout: Duration::from_secs(60),
        min_body_rate: None,
    };
    let store = S3Store::new(config).map_err(|e| e.to_string())?;
    let prefix = KeyPrefix::new(prefix).map_err(|e| e.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let (mut count, mut after): (u64, Option<ListToken>) = (0, None);
        loop {
            let listed = store.list(&prefix, after.as_ref(), page).await;
            let listed = listed.map_err(|e| e.to_string())?;
            count += u64::try_from(listed.objects.len()).unwrap_or(u64::MAX);
            after = listed.next;
            if after.is_none() {
                return Ok(count);
            }
        }
    })
}

/// The text of `path`.
fn read(path: &Path) -> Result<String, Error> {
    fs::read_to_string(path).map_err(io_at(path))
}

/// Makes `work` if it is not there, refuses it if it holds anything, and makes its `home`, `tmp`
/// and `logs`. Gives its absolute path.
fn prepare(work: &Path) -> Result<PathBuf, Error> {
    fs::create_dir_all(work).map_err(io_at(work))?;
    let mut entries = fs::read_dir(work).map_err(io_at(work))?;
    if entries.next().is_some() {
        let path = work.to_path_buf();
        return Err(Error::WorkNotEmpty { path });
    }
    let work = fs::canonicalize(work).map_err(io_at(work))?;
    for dir in ["home", "tmp", "logs"] {
        let path = work.join(dir);
        fs::create_dir(&path).map_err(io_at(&path))?;
    }
    Ok(work)
}

/// A free loopback port, and the listener that holds it until it is dropped.
fn free_port(work: &Path) -> Result<(u16, TcpListener), Error> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(io_at(work))?;
    let port = listener.local_addr().map_err(io_at(work))?.port();
    Ok((port, listener))
}

/// The `server.bucket` of a tierstore configuration.
fn bucket_of(config: &str) -> Result<String, String> {
    let table: toml::Table = config
        .parse()
        .map_err(|e: toml::de::Error| e.message().to_owned())?;
    let bucket = table
        .get("server")
        .and_then(|s| s.get("bucket"))
        .and_then(|b| b.as_str());
    bucket
        .map(str::to_owned)
        .ok_or_else(|| "it has no server.bucket".to_owned())
}

/// Writes `text` to the new file `path`, readable by its owner alone (as tierstore wants its key
/// file).
fn write_private(path: &Path, text: &str) -> Result<(), Error> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(io_at(path))?;
    file.write_all(text.as_bytes()).map_err(io_at(path))
}
