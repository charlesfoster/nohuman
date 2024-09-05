use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Parser;
use env_logger::Builder;
use lazy_static::lazy_static;
use log::{debug, error, info, warn, LevelFilter};
use nohuman::{
    check_path_exists, download::download_database, validate_db_directory, compress_output, CommandRunner,
};
use std::process::{Command, Stdio};
use std::fs::File;
use std::io::Write;

lazy_static! {
    static ref DEFAULT_DB_LOCATION: String = {
        let home = dirs::home_dir().expect("Could not find home directory");
        home.join(".nohuman")
            .join("db")
            .to_str()
            .unwrap()
            .to_string()
    };
}

/// Struct representing the command-line arguments
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Input file(s) to remove human reads from.
    ///
    /// This is a required argument unless `--check` or `--download` is specified.
    #[arg(
        name = "INPUT",
        required_unless_present_any = &["check", "download"],
        value_parser = check_path_exists,
        verbatim_doc_comment
    )]
    input: Option<Vec<PathBuf>>,

    /// First output file.
    ///
    /// Defaults to the name of the first input file with the suffix "nohuman" appended.
    /// e.g., "input_1.fastq.gz" -> "input_1.nohuman.fq.gz". 
    /// If the file stem is `.gz`, the output will be compressed.
    #[arg(
        short,
        long,
        name = "OUTPUT_1",
        verbatim_doc_comment
    )]
    pub out1: Option<PathBuf>,

    /// Second output file.
    ///
    /// Defaults to the name of the second input file with the suffix "nohuman" appended.
    /// e.g., "input_2.fastq.gz" -> "input_2.nohuman.fq.gz". 
    /// If the file stem is `.gz`, the output will be compressed.
    #[arg(
        short = 'O',
        long,
        name = "OUTPUT_2",
        verbatim_doc_comment
    )]
    pub out2: Option<PathBuf>,

    /// Check that all required dependencies are available and exit.
    #[arg(
        short,
        long,
        verbatim_doc_comment
    )]
    check: bool,

    /// Download the database required for the process.
    #[arg(
        short,
        long,
        verbatim_doc_comment
    )]
    download: bool,

    /// Path to the database.
    ///
    /// Defaults to the database location specified in the home directory.
    #[arg(
        short = 'D',
        long = "db",
        value_name = "PATH",
        default_value = &**DEFAULT_DB_LOCATION,
        verbatim_doc_comment
    )]
    database: PathBuf,

    /// Write `kraken2` logging information to filename specified here.
    ///
    /// If not specified, no `kraken2` log is saved.
    #[arg(
        short = 'l',
        long = "kraken2-log",
        value_name = "PATH",
        verbatim_doc_comment
    )]
    kraken2_log: Option<PathBuf>,

    /// Number of threads to use in kraken2 (and compression, if applicable)
    #[arg(
        short,
        long,
        value_name = "INT",
        default_value_t = 1,
        verbatim_doc_comment
    )]
    threads: usize,

    /// Set the `nohuman` logging level to verbose
    #[arg(
        short,
        long,
        verbatim_doc_comment
    )]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logger
    let log_lvl = if args.verbose {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };
    let mut log_builder = Builder::new();
    log_builder
        .filter(None, log_lvl)
        .filter_module("reqwest", LevelFilter::Off)
        .format_module_path(false)
        .format_target(false)
        .init();

    // Check if the database exists
    if !args.database.exists() && !args.download && !args.check {
        bail!("Database does not exist. Use --download to download the database");
    }

    if args.download {
        info!("Downloading database...");
        download_database(&args.database).context("Failed to download database")?;
        info!("Database downloaded");
        if args.input.is_none() {
            info!("No input files provided. Exiting.");
            return Ok(());
        }
    }

    let kraken = CommandRunner::new("kraken2");

    let external_commands = vec![&kraken];

    let mut missing_commands = Vec::new();
    for cmd in external_commands {
        if !cmd.is_executable() {
            debug!("{} is not executable", cmd.command);
            missing_commands.push(cmd.command.to_owned());
        } else {
            debug!("{} is executable", cmd.command);
        }
    }

    if !missing_commands.is_empty() {
        error!("The following dependencies are missing:");
        for cmd in missing_commands {
            error!("{}", cmd);
        }
        bail!("Missing dependencies");
    }

    if args.check {
        info!("All dependencies are available");
        return Ok(());
    }

    // error out if input files are not provided, otherwise unwrap to a variable
    let input = args.input.context("No input files provided")?;

    let temp_kraken_output =
        tempfile::NamedTempFile::new().context("Failed to create temporary kraken output file")?;
    let threads = args.threads.to_string();
    let db = validate_db_directory(&args.database)
        .map_err(|e| anyhow::anyhow!(e))?
        .to_string_lossy()
        .to_string();
    let mut kraken_cmd = vec![
        "--threads",
        &threads,
        "--db",
        &db,
        "--output",
        temp_kraken_output.path().to_str().unwrap(),
    ];
    match input.len() {
        2 => kraken_cmd.push("--paired"),
        i if i > 2 => bail!("Only one or two input files are allowed"),
        _ => {}
    }

    // create a temporary output directory in the current directory and don't delete it
    let tmpdir = tempfile::Builder::new()
        .prefix("nohuman")
        .tempdir_in(std::env::current_dir().unwrap())
        .context("Failed to create temporary directory")?;
    let outfile = if input.len() == 2 {
        tmpdir.path().join("kraken_out#.fq")
    } else {
        tmpdir.path().join("kraken_out.fq")
    };
    let outfile = outfile.to_string_lossy().to_string();
    kraken_cmd.extend(&["--unclassified-out", &outfile]);

    kraken_cmd.extend(input.iter().map(|p| p.to_str().unwrap()));
    info!("Running kraken2...");
    debug!("With arguments: {:?}", &kraken_cmd);

    // Run the kraken2 command and capture stdout/stderr
    let kraken_run = Command::new("kraken2")  // Replace "kraken2" with the actual command you run
        .args(&kraken_cmd)                    // Pass arguments to the command
        .stdout(Stdio::piped())               // Capture stdout
        .stderr(Stdio::piped())               // Capture stderr
        .output()                             // Execute the command and capture output
        .context("Failed to run kraken2")?;

    // Write stderr (= kraken2 logging info) to a log file
    if let Some(log_path) = &args.kraken2_log {
        let mut log_file = File::create(log_path).context("Failed to create log file")?;
        log_file.write_all(&kraken_run.stderr).context("Failed to write `kraken2` stderr to log file")?;
    }

    info!("Kraken2 finished. Organising output...");

    if input.len() == 2 {
        let out1 = args.out1.unwrap_or_else(|| {
            let parent = input[0].parent().unwrap();
            let fname = if input[0].extension().unwrap_or_default() == "gz" {
                // If .gz extension, remove both .gz and the prior extension
                let no_ext = input[0].with_extension("");   // Strip .gz
                let stem = no_ext.file_stem().unwrap();     // Strip the original extension (e.g., .fastq)
                format!("{}.nohuman.fq.gz", stem.to_string_lossy()) // Append .nohuman.fq.gz
            } else {
                // If no .gz extension, just append .nohuman.fq
                format!("{}.nohuman.fq", input[0].file_stem().unwrap().to_string_lossy())
            };
            parent.join(fname)
        });
        let out2 = args.out2.unwrap_or_else(|| {
            let parent = input[1].parent().unwrap();
            let fname = if input[1].extension().unwrap_or_default() == "gz" {
                // If .gz extension, remove both .gz and the prior extension
                let no_ext = input[1].with_extension("");   // Strip .gz
                let stem = no_ext.file_stem().unwrap();     // Strip the original extension (e.g., .fastq)
                format!("{}.nohuman.fq.gz", stem.to_string_lossy()) // Append .nohuman.fq.gz
            } else {
                // If no .gz extension, just append .nohuman.fq
                format!("{}.nohuman.fq", input[1].file_stem().unwrap().to_string_lossy())
            };
            parent.join(fname)
        });
        
        let tmpout1 = tmpdir.path().join("kraken_out_1.fq");
        let tmpout2 = tmpdir.path().join("kraken_out_2.fq");
    
        // Check if the output file should be compressed
        if out1.extension().unwrap_or_default() == "gz" {
            compress_output(&tmpout1, &out1, args.threads)?;
        } else {
            std::fs::rename(tmpout1.clone(), &out1).unwrap();  // Cloning the PathBuf here
        }
        
        if out2.extension().unwrap_or_default() == "gz" {
            compress_output(&tmpout2, &out2, args.threads)?;
        } else {
            std::fs::rename(tmpout2.clone(), &out2).unwrap();  // Cloning tmpout2 if needed
        }

        info!("Output files written to: {:?} and {:?}", &out1, &out2);
    } else {
        let out1 = args.out1.unwrap_or_else(|| {
            let parent = input[0].parent().unwrap();
            let fname = if input[0].extension().unwrap_or_default() == "gz" {
                let no_ext = input[0].with_extension("");
                no_ext.file_stem().unwrap().to_owned()
            } else {
                input[0].file_stem().unwrap().to_owned()
            };
            let fname = format!("{}.nohuman.fq", fname.to_string_lossy());
            parent.join(fname)
        });
        
        let tmpout1 = tmpdir.path().join("kraken_out.fq");
        
        // Check if the output file should be compressed
        if out1.extension().unwrap_or_default() == "gz" {
            compress_output(&tmpout1, &out1, args.threads)?;
        } else {
            std::fs::rename(tmpout1, &out1).unwrap();
        }
        
        info!("Output file written to: {:?}", &out1);
    }

    // cleanup the temporary directory, but only issue a warning if it fails
    if let Err(e) = tmpdir.close() {
        warn!("Failed to remove temporary output directory: {}", e);
    }

    info!("Done.");

    Ok(())
}
