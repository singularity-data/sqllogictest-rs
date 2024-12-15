mod engines;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{stdout, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;
use clap::{Arg, ArgAction, CommandFactory, FromArgMatches, Parser, ValueEnum};
use console::style;
use engines::{EngineConfig, EngineType};
use fs_err::{File, OpenOptions};
use futures::StreamExt;
use itertools::Itertools;
use quick_junit::{NonSuccessKind, Report, TestCase, TestCaseStatus, TestSuite};
use rand::distributions::DistString;
use rand::seq::SliceRandom;
use sqllogictest::{
    default_column_validator, default_normalizer, default_validator, update_record_with_output,
    AsyncDB, Injected, MakeConnection, Record, Runner,
};

#[derive(Default, Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[must_use]
pub enum Color {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Parser, Debug, Clone)]
#[clap(about, version, author)]
struct Opt {
    /// Glob(s) of a set of test files.
    /// For example: `./test/**/*.slt`
    #[clap(required = true, num_args = 1..)]
    files: Vec<String>,

    /// The database engine name, used by the record conditions.
    #[clap(short, long, value_enum, default_value = "postgres")]
    engine: EngineType,

    /// Example: "java -cp a.jar com.risingwave.sqllogictest.App
    /// jdbc:postgresql://{host}:{port}/{db} {user}" The items in `{}` will be replaced by
    /// [`DBConfig`].
    #[clap(long, env)]
    external_engine_command_template: Option<String>,

    /// Whether to enable colorful output.
    #[clap(
        long,
        value_enum,
        default_value_t,
        value_name = "WHEN",
        env = "CARGO_TERM_COLOR"
    )]
    color: Color,

    /// Whether to enable parallel test. The `db` option will be used to create databases, and one
    /// database will be created for each test file.
    #[clap(long, short)]
    jobs: Option<usize>,
    /// When using `-j`, whether to keep the temporary database when a test case fails.
    #[clap(long, default_value = "false")]
    keep_db_on_failure: bool,

    /// Report to junit XML.
    #[clap(long)]
    junit: Option<String>,

    /// The database server host.
    /// If multiple addresses are specified, one will be chosen randomly per session.
    #[clap(short, long, default_value = "localhost", env = "SLT_HOST")]
    host: Vec<String>,
    /// The database server port.
    /// If multiple addresses are specified, one will be chosen randomly per session.
    #[clap(short, long, default_value = "5432", env = "SLT_PORT")]
    port: Vec<u16>,
    /// The database name to connect.
    #[clap(short, long, default_value = "postgres", env = "SLT_DB")]
    db: String,
    /// The database username.
    #[clap(short, long, default_value = "postgres", env = "SLT_USER")]
    user: String,
    /// The database password.
    #[clap(short = 'w', long, default_value = "postgres", env = "SLT_PASSWORD")]
    pass: String,
    /// The database options.
    #[clap(long)]
    options: Option<String>,

    /// Overrides the test files with the actual output of the database.
    #[clap(long)]
    r#override: bool,
    /// Reformats the test files.
    #[clap(long)]
    format: bool,

    /// Add a label for conditions.
    ///
    /// Records with `skipif label` will be skipped if the label is present.
    /// Records with `onlyif label` will be executed only if the label is present.
    ///
    /// The engine name is a label by default.
    #[clap(long = "label")]
    labels: Vec<String>,
}

/// Connection configuration.
#[derive(Clone)]
struct DBConfig {
    /// The database server host and port. Will randomly choose one if multiple are given.
    addrs: Vec<(String, u16)>,
    /// The database name to connect.
    db: String,
    /// The database username.
    user: String,
    /// The database password.
    pass: String,
    /// Command line options.
    options: Option<String>,
}

impl DBConfig {
    fn random_addr(&self) -> (&str, u16) {
        self.addrs
            .choose(&mut rand::thread_rng())
            .map(|(host, port)| (host.as_ref(), *port))
            .unwrap()
    }
}

#[tokio::main]
pub async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Opt::command().disable_help_flag(true).arg(
        Arg::new("help")
            .long("help")
            .help("Print help information")
            .action(ArgAction::Help),
    );
    let matches = cli.get_matches();
    let Opt {
        files,
        engine,
        external_engine_command_template,
        color,
        jobs,
        keep_db_on_failure,
        junit,
        host,
        port,
        db,
        user,
        pass,
        options,
        r#override,
        format,
        labels,
    } = Opt::from_arg_matches(&matches)
        .map_err(|err| err.exit())
        .unwrap();

    if host.len() != port.len() {
        bail!(
            "{} hosts are provided while {} ports are provided",
            host.len(),
            port.len(),
        );
    }
    let addrs = host.into_iter().zip_eq(port).collect();

    let engine = match engine {
        EngineType::Mysql => EngineConfig::MySql,
        EngineType::Postgres => EngineConfig::Postgres,
        EngineType::PostgresExtended => EngineConfig::PostgresExtended,
        EngineType::External => {
            if let Some(external_engine_command_template) = external_engine_command_template {
                EngineConfig::External(external_engine_command_template)
            } else {
                bail!("`--external-engine-command-template` is required for `--engine=external`")
            }
        }
    };

    match color {
        Color::Always => {
            console::set_colors_enabled(true);
            console::set_colors_enabled_stderr(true);
        }
        Color::Never => {
            console::set_colors_enabled(false);
            console::set_colors_enabled_stderr(false);
        }
        Color::Auto => {}
    }

    let glob_patterns = files;
    let mut files: Vec<PathBuf> = Vec::new();
    for glob_pattern in glob_patterns.into_iter() {
        let pathbufs = glob::glob(&glob_pattern).context("failed to read glob pattern")?;
        for pathbuf in pathbufs.into_iter().try_collect::<_, Vec<_>, _>()? {
            files.push(pathbuf)
        }
    }

    if files.is_empty() {
        bail!("no test case found");
    }

    let config = DBConfig {
        addrs,
        db,
        user,
        pass,
        options,
    };

    if r#override || format {
        return update_test_files(files, &engine, config, format).await;
    }

    let mut report = Report::new(junit.clone().unwrap_or_else(|| "sqllogictest".to_string()));
    report.set_timestamp(Local::now());

    let mut test_suite = TestSuite::new("sqllogictest");
    test_suite.set_timestamp(Local::now());

    let result = if let Some(jobs) = jobs {
        run_parallel(
            jobs,
            keep_db_on_failure,
            &mut test_suite,
            files,
            &engine,
            config,
            &labels,
            junit.clone(),
        )
        .await
    } else {
        run_serial(
            &mut test_suite,
            files,
            &engine,
            config,
            &labels,
            junit.clone(),
        )
        .await
    };

    report.add_test_suite(test_suite);

    if let Some(junit_file) = junit {
        tokio::fs::write(format!("{junit_file}-junit.xml"), report.to_string()?).await?;
    }

    result
}

#[allow(clippy::too_many_arguments)]
async fn run_parallel(
    jobs: usize,
    keep_db_on_failure: bool,
    test_suite: &mut TestSuite,
    files: Vec<PathBuf>,
    engine: &EngineConfig,
    config: DBConfig,
    labels: &[String],
    junit: Option<String>,
) -> Result<()> {
    let mut create_databases = BTreeMap::new();
    let mut filenames = BTreeSet::new();
    for file in files {
        let filename = file
            .to_str()
            .ok_or_else(|| anyhow!("not a UTF-8 filename"))?;
        let normalized_filename = filename.replace([' ', '.', '-', '/'], "_");
        eprintln!("+ Discovered Test: {normalized_filename}");
        if !filenames.insert(normalized_filename.clone()) {
            return Err(anyhow!(
                "duplicated file name found: {}",
                normalized_filename
            ));
        }
        let random_id: String = rand::distributions::Alphanumeric
            .sample_string(&mut rand::thread_rng(), 8)
            .to_lowercase();
        let db_name = format!("{normalized_filename}_{random_id}");

        create_databases.insert(db_name, file);
    }

    let mut db = engines::connect(engine, &config).await?;

    let db_names: Vec<String> = create_databases.keys().cloned().collect();
    for db_name in &db_names {
        let query = format!("CREATE DATABASE {db_name};");
        eprintln!("+ {query}");
        if let Err(err) = db.run(&query).await {
            eprintln!("  ignore error: {err}");
        }
    }

    let mut stream = futures::stream::iter(create_databases.into_iter())
        .map(|(db_name, filename)| {
            let mut config = config.clone();
            config.db.clone_from(&db_name);
            let file = filename.to_string_lossy().to_string();
            let engine = engine.clone();
            let labels = labels.to_vec();
            async move {
                let (buf, res) = tokio::spawn(async move {
                    let mut buf = vec![];
                    let res =
                        connect_and_run_test_file(&mut buf, filename, &engine, config, &labels)
                            .await;
                    (buf, res)
                })
                .await
                .unwrap();
                (db_name, file, res, buf)
            }
        })
        .buffer_unordered(jobs);

    eprintln!("{}", style("[TEST IN PROGRESS]").blue().bold());

    let mut failed_case = vec![];
    let mut failed_db: HashSet<String> = HashSet::new();

    let start = Instant::now();

    while let Some((db_name, file, res, mut buf)) = stream.next().await {
        let test_case_name = file.replace(['/', ' ', '.', '-'], "_");
        let case = match res {
            Ok(duration) => {
                let mut case = TestCase::new(test_case_name, TestCaseStatus::success());
                case.set_time(duration);
                case.set_timestamp(Local::now());
                case.set_classname(junit.as_deref().unwrap_or_default());
                case
            }
            Err(e) => {
                writeln!(buf, "{}\n\n{:?}", style("[FAILED]").red().bold(), e)?;
                writeln!(buf)?;
                failed_case.push(file.clone());
                failed_db.insert(db_name.clone());
                let mut status = TestCaseStatus::non_success(NonSuccessKind::Failure);
                status.set_type("test failure");
                let mut case = TestCase::new(test_case_name, status);
                case.set_system_err(e.to_string());
                case.set_time(Duration::from_millis(0));
                case.set_system_out("");
                case.set_timestamp(Local::now());
                case.set_classname(junit.as_deref().unwrap_or_default());
                case
            }
        };
        test_suite.add_test_case(case);
        tokio::task::block_in_place(|| stdout().write_all(&buf))?;
    }

    eprintln!(
        "\n All test cases finished in {} ms",
        start.elapsed().as_millis()
    );

    for db_name in db_names {
        if keep_db_on_failure && failed_db.contains(&db_name) {
            eprintln!(
                "+ {}",
                style(format!(
                    "DATABASE {db_name} contains failed cases, kept for debugging"
                ))
                .red()
                .bold()
            );
            continue;
        }
        let query = format!("DROP DATABASE {db_name};");
        eprintln!("+ {query}");
        if let Err(err) = db.run(&query).await {
            eprintln!("  ignore error: {err}");
        }
    }

    if !failed_case.is_empty() {
        Err(anyhow!("some test case failed:\n{:#?}", failed_case))
    } else {
        Ok(())
    }
}

// Run test one be one
async fn run_serial(
    test_suite: &mut TestSuite,
    files: Vec<PathBuf>,
    engine: &EngineConfig,
    config: DBConfig,
    labels: &[String],
    junit: Option<String>,
) -> Result<()> {
    let mut failed_case = vec![];

    for file in files {
        let mut runner = Runner::new(|| engines::connect(engine, &config));
        for label in labels {
            runner.add_label(label);
        }

        let filename = file.to_string_lossy().to_string();
        let test_case_name = filename.replace(['/', ' ', '.', '-'], "_");
        let case = match run_test_file(&mut std::io::stdout(), runner, &file).await {
            Ok(duration) => {
                let mut case = TestCase::new(test_case_name, TestCaseStatus::success());
                case.set_time(duration);
                case.set_timestamp(Local::now());
                case.set_classname(junit.as_deref().unwrap_or_default());
                case
            }
            Err(e) => {
                println!("{}\n\n{:?}", style("[FAILED]").red().bold(), e);
                println!();
                failed_case.push(filename.clone());
                let mut status = TestCaseStatus::non_success(NonSuccessKind::Failure);
                status.set_type("test failure");
                let mut case = TestCase::new(test_case_name, status);
                case.set_timestamp(Local::now());
                case.set_classname(junit.as_deref().unwrap_or_default());
                case.set_system_err(e.to_string());
                case.set_time(Duration::from_millis(0));
                case.set_system_out("");
                case
            }
        };
        test_suite.add_test_case(case);
    }

    if !failed_case.is_empty() {
        Err(anyhow!("some test case failed:\n{:#?}", failed_case))
    } else {
        Ok(())
    }
}

/// * `format` - If true, will not run sqls, only formats the file.
async fn update_test_files(
    files: Vec<PathBuf>,
    engine: &EngineConfig,
    config: DBConfig,
    format: bool,
) -> Result<()> {
    for file in files {
        let runner = Runner::new(|| engines::connect(engine, &config));

        if let Err(e) = update_test_file(&mut std::io::stdout(), runner, &file, format).await {
            {
                println!("{}\n\n{:?}", style("[FAILED]").red().bold(), e);
                println!();
            }
        };
    }

    Ok(())
}

async fn flush(out: &mut impl std::io::Write) -> std::io::Result<()> {
    tokio::task::block_in_place(|| out.flush())
}

async fn connect_and_run_test_file(
    out: &mut impl std::io::Write,
    filename: PathBuf,
    engine: &EngineConfig,
    config: DBConfig,
    labels: &[String],
) -> Result<Duration> {
    let mut runner = Runner::new(|| engines::connect(engine, &config));
    for label in labels {
        runner.add_label(label);
    }
    let result = run_test_file(out, runner, filename).await?;

    Ok(result)
}

/// Different from [`Runner::run_file_async`], we re-implement it here to print some progress
/// information.
async fn run_test_file<T: std::io::Write, M: MakeConnection>(
    out: &mut T,
    mut runner: Runner<M::Conn, M>,
    filename: impl AsRef<Path>,
) -> Result<Duration> {
    let filename = filename.as_ref();
    let records =
        tokio::task::block_in_place(|| sqllogictest::parse_file(filename).map_err(|e| anyhow!(e)))
            .context("failed to parse sqllogictest file")?;

    let mut begin_times = vec![];
    let mut did_pop = false;

    write!(out, "{: <60} .. ", filename.to_string_lossy())?;
    flush(out).await?;

    begin_times.push(Instant::now());

    for record in records {
        if let Record::Halt { .. } = record {
            break;
        }
        match &record {
            Record::Injected(Injected::BeginInclude(file)) => {
                begin_times.push(Instant::now());
                if !did_pop {
                    writeln!(out, "{}", style("[BEGIN]").blue().bold())?;
                } else {
                    writeln!(out)?;
                }
                did_pop = false;
                write!(
                    out,
                    "{}{: <60} .. ",
                    "| ".repeat(begin_times.len() - 1),
                    file
                )?;
                flush(out).await?;
            }
            Record::Injected(Injected::EndInclude(file)) => {
                finish_test_file(out, &mut begin_times, &mut did_pop, file)?;
            }
            _ => {}
        }

        runner
            .run_async(record)
            .await
            .map_err(|e| anyhow!("{}", e.display(console::colors_enabled())))
            .context(format!(
                "failed to run `{}`",
                style(filename.to_string_lossy()).bold()
            ))?;
    }

    let duration = begin_times[0].elapsed();

    finish_test_file(
        out,
        &mut begin_times,
        &mut did_pop,
        &filename.to_string_lossy(),
    )?;

    writeln!(out)?;

    Ok(duration)
}

fn finish_test_file<T: std::io::Write>(
    out: &mut T,
    time_stack: &mut Vec<Instant>,
    did_pop: &mut bool,
    file: &str,
) -> Result<()> {
    let begin_time = time_stack.pop().unwrap();

    if *did_pop {
        // start a new line if the result is not immediately after the item
        write!(
            out,
            "\n{}{} {: <54} .. {} in {} ms",
            "| ".repeat(time_stack.len()),
            style("[END]").blue().bold(),
            file,
            style("[OK]").green().bold(),
            begin_time.elapsed().as_millis()
        )?;
    } else {
        // otherwise, append time to the previous line
        write!(
            out,
            "{} in {} ms",
            style("[OK]").green().bold(),
            begin_time.elapsed().as_millis()
        )?;
    }

    *did_pop = true;

    Ok::<_, anyhow::Error>(())
}

/// Different from [`sqllogictest::update_test_file`], we re-implement it here to print some
/// progress information.
async fn update_test_file<T: std::io::Write, M: MakeConnection>(
    out: &mut T,
    mut runner: Runner<M::Conn, M>,
    filename: impl AsRef<Path>,
    format: bool,
) -> Result<()> {
    let filename = filename.as_ref();
    let records = tokio::task::block_in_place(|| {
        sqllogictest::parse_file(filename).map_err(|e| anyhow!("{:?}", e))
    })
    .context("failed to parse sqllogictest file")?;

    let mut begin_times = vec![];
    let mut did_pop = false;

    write!(out, "{: <60} .. ", filename.to_string_lossy())?;
    flush(out).await?;

    begin_times.push(Instant::now());

    fn create_outfile(filename: impl AsRef<Path>) -> std::io::Result<(PathBuf, File)> {
        let filename = filename.as_ref();
        let outfilename = filename.file_name().unwrap().to_str().unwrap().to_owned() + ".temp";
        let outfilename = filename.parent().unwrap().join(outfilename);
        // create a temp file in read-write mode
        let outfile = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .read(true)
            .open(&outfilename)?;
        Ok((outfilename, outfile))
    }

    fn override_with_outfile(
        filename: &String,
        outfilename: &PathBuf,
        outfile: &mut File,
    ) -> std::io::Result<()> {
        // check whether outfile ends with multiple newlines, which happens if
        // - the last record is statement/query
        // - the original file ends with multiple newlines

        const N: usize = 8;
        let mut buf = [0u8; N];
        loop {
            outfile.seek(SeekFrom::End(-(N as i64))).unwrap();
            outfile.read_exact(&mut buf).unwrap();
            let num_newlines = buf.iter().rev().take_while(|&&b| b == b'\n').count();
            assert!(num_newlines > 0);

            if num_newlines > 1 {
                // if so, remove the last ones
                outfile
                    .set_len(outfile.metadata().unwrap().len() - num_newlines as u64 + 1)
                    .unwrap();
            }

            if num_newlines == 1 || num_newlines < N {
                break;
            }
        }

        fs_err::rename(outfilename, filename)?;

        Ok(())
    }

    struct Item {
        filename: String,
        outfilename: PathBuf,
        outfile: File,
        halt: bool,
    }
    let (outfilename, outfile) = create_outfile(filename)?;
    let mut stack = vec![Item {
        filename: filename.to_string_lossy().to_string(),
        outfilename,
        outfile,
        halt: false,
    }];

    for record in records {
        let Item {
            filename,
            outfilename,
            outfile,
            halt,
        } = stack.last_mut().unwrap();

        match &record {
            Record::Injected(Injected::BeginInclude(filename)) => {
                let (outfilename, outfile) = create_outfile(filename)?;
                stack.push(Item {
                    filename: filename.clone(),
                    outfilename,
                    outfile,
                    halt: false,
                });

                begin_times.push(Instant::now());
                if !did_pop {
                    writeln!(out, "{}", style("[BEGIN]").blue().bold())?;
                } else {
                    writeln!(out)?;
                }
                did_pop = false;
                write!(
                    out,
                    "{}{: <60} .. ",
                    "| ".repeat(begin_times.len() - 1),
                    filename
                )?;
                flush(out).await?;
            }
            Record::Injected(Injected::EndInclude(file)) => {
                override_with_outfile(filename, outfilename, outfile)?;
                stack.pop();
                finish_test_file(out, &mut begin_times, &mut did_pop, file)?;
            }
            _ => {
                if *halt {
                    writeln!(outfile, "{record}")?;
                    continue;
                }
                if matches!(record, Record::Halt { .. }) {
                    *halt = true;
                    writeln!(outfile, "{record}")?;
                    continue;
                }
                update_record(outfile, &mut runner, record, format)
                    .await
                    .context(format!("failed to run `{}`", style(filename).bold()))?;
            }
        }
    }

    finish_test_file(
        out,
        &mut begin_times,
        &mut did_pop,
        &filename.to_string_lossy(),
    )?;

    let Item {
        filename,
        outfilename,
        outfile,
        halt: _,
    } = stack.last_mut().unwrap();
    override_with_outfile(filename, outfilename, outfile)?;

    Ok(())
}

async fn update_record<M: MakeConnection>(
    outfile: &mut File,
    runner: &mut Runner<M::Conn, M>,
    record: Record<<M::Conn as AsyncDB>::ColumnType>,
    format: bool,
) -> Result<()> {
    assert!(!matches!(record, Record::Injected(_)));

    if format {
        writeln!(outfile, "{record}")?;
        return Ok(());
    }

    let record_output = runner.apply_record(record.clone()).await;
    match update_record_with_output(
        &record,
        &record_output,
        "\t",
        default_validator,
        default_normalizer,
        default_column_validator,
    ) {
        Some(new_record) => {
            writeln!(outfile, "{new_record}")?;
        }
        None => {
            writeln!(outfile, "{record}")?;
        }
    }

    Ok(())
}
