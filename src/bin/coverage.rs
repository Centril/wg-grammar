extern crate gll;
extern crate proc_macro2;
extern crate rayon;
extern crate rust_grammar;
extern crate structopt;
extern crate walkdir;
#[macro_use]
extern crate derive_more;
#[macro_use]
extern crate serde_derive;
extern crate toml;

use gll::runtime::{MoreThanOne, ParseNodeKind, ParseNodeShape};
use rayon::prelude::*;
use rust_grammar::parse;
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use structopt::StructOpt;
use walkdir::WalkDir;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Blacklist {
    paths: Vec<String>,
}

impl Blacklist {
    fn is_blacklisted(&self, path: &Path) -> bool {
        self.paths.iter().any(|ref b| path.ends_with(b))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Config {
    blacklist: Blacklist,
}

impl Config {
    fn load() -> Result<Config, failure::Error> {
        let config = match fs::read_to_string("wg-grammar.toml") {
            Ok(mut toml) => toml::from_str(&toml)?,
            Err(_) => Config::default(),
        };
        Ok(config)
    }
}

#[derive(StructOpt)]
enum Command {
    #[structopt(name = "file")]
    /// Test parsing an individual Rust file
    File {
        #[structopt(parse(from_os_str), long = "graphviz-forest")]
        /// Dump the internal parse forest as a Graphviz .dot file
        graphviz_forest: Option<PathBuf>,

        #[structopt(parse(from_os_str))]
        /// Rust input file
        file: PathBuf,
    },

    #[structopt(name = "dir")]
    /// Test parsing a directory of Rust files
    Dir {
        #[structopt(short = "v", long = "verbose")]
        /// Print information about each file on stderr
        verbose: bool,

        #[structopt(parse(from_os_str))]
        /// Directory to find Rust files in
        dir: PathBuf,
    },
}

type ModuleContentsResult<'a, 'i> = parse::ParseResult<
    'a,
    'i,
    proc_macro2::TokenStream,
    parse::ModuleContents<'a, 'i, proc_macro2::TokenStream>,
>;

type ModuleContentsHandle<'a, 'i> = parse::Handle<
    'a,
    'i,
    proc_macro2::TokenStream,
    parse::ModuleContents<'a, 'i, proc_macro2::TokenStream>,
>;

/// Read the contents of the file at the given `path`, parse it
/// using the `ModuleContents` rule, and pass the result to `f`.
fn parse_file_with<R>(path: &Path, f: impl FnOnce(ModuleContentsResult) -> R) -> R {
    let src = fs::read_to_string(path).unwrap();
    match src.parse::<proc_macro2::TokenStream>() {
        Ok(tts) => parse::ModuleContents::parse_with(tts, |_, result| f(result)),
        // FIXME(eddyb) provide more information in this error case.
        Err(_) => f(Err(parse::ParseError::NoParse)),
    }
}

/// Output the result of a single file to stderr,
/// optionally prefixed by a given `path`.
fn report_file_result(
    path: Option<&Path>,
    result: ModuleContentsResult,
    ambiguity_result: Result<(), MoreThanOne>,
    duration: Option<Duration>,
) {
    if let Some(duration) = duration {
        eprint!("{:?}: ", duration);
    }

    if let Some(path) = path {
        eprint!("{}: ", path.display());
    }
    // Avoid printing too much, especially not any parse nodes.
    match (result, ambiguity_result) {
        (Ok(_), Ok(_)) => eprintln!("OK"),
        (Ok(_), Err(_)) => eprintln!("OK (ambiguous)"),
        (Err(parse::ParseError::TooShort(handle)), _) => {
            eprint!("FAIL after ");

            #[cfg(procmacro2_semver_exempt)]
            {
                // HACK(eddyb) work around `proc-macro2` `Span` printing limitation
                let end_location = handle.source_info().end.end();
                eprintln!("{}:{}", end_location.line, end_location.column);
            }
            #[cfg(not(procmacro2_semver_exempt))]
            {
                let _ = handle;
                eprintln!(
                    "(missing location information; \
                     set `RUSTFLAGS='--cfg procmacro2_semver_exempt'`)"
                );
            }
        }
        (Err(parse::ParseError::NoParse), _) => eprintln!("FAIL (lexer error?)"),
    }
}

fn ambiguity_check(handle: ModuleContentsHandle) -> Result<(), MoreThanOne> {
    let sppf = &handle.parser.sppf;

    let mut queue = VecDeque::new();
    queue.push_back(handle.node);
    let mut seen: BTreeSet<_> = queue.iter().cloned().collect();

    while let Some(source) = queue.pop_front() {
        let mut add_children = |children: &[_]| {
            for &child in children {
                if seen.insert(child) {
                    queue.push_back(child);
                }
            }
        };
        match source.kind.shape() {
            ParseNodeShape::Opaque => {}
            ParseNodeShape::Alias(_) => add_children(&[source.unpack_alias()]),
            ParseNodeShape::Opt(_) => {
                if let Some(child) = source.unpack_opt() {
                    add_children(&[child]);
                }
            }
            ParseNodeShape::Choice => add_children(&[sppf.one_choice(source)?]),
            ParseNodeShape::Split(..) => {
                let (left, right) = sppf.one_split(source)?;
                add_children(&[left, right])
            }
        }
    }

    Ok(())
}

#[derive(Debug, Default, Add)]
struct Counters {
    total_count: u16,
    unambiguous_count: u16,
    ambiguous_count: u16,
    too_short_count: u16,
    no_parse_count: u16,
}

#[derive(Debug)]
enum ParseResult {
    Unambiguous,
    Ambiguous,
    Partial,
    Error,
}

impl ParseResult {
    fn compact_display(&self) -> &'static str {
        match self {
            ParseResult::Unambiguous => "-",
            ParseResult::Ambiguous => ".",
            ParseResult::Partial => "X",
            ParseResult::Error => "L",
        }
    }
}

fn process(file: walkdir::DirEntry, verbose: bool) -> ParseResult {
    let mut stdout = io::stdout();
    let path = file.into_path();

    let out = parse_file_with(&path, |result| {
        let mut ambiguity_result = Ok(());
        let start = Instant::now();
        let status = match result {
            Ok(handle) => {
                ambiguity_result = ambiguity_check(handle);
                if ambiguity_result.is_ok() {
                    ParseResult::Unambiguous
                } else {
                    ParseResult::Ambiguous
                }
            }
            Err(parse::ParseError::TooShort(_)) => ParseResult::Partial,
            Err(parse::ParseError::NoParse) => ParseResult::Error,
        };
        let duration = start.elapsed();
        if verbose {
            report_file_result(Some(&path), result, ambiguity_result, Some(duration));
        } else {
            print!("{}", status.compact_display());
            stdout.flush().unwrap();
        }
        status
    });

    out
}

fn print_statistics(counters: Counters) {
    println!("");
    println!("Out of {} Rust files tested:", counters.total_count);
    println!(
        "* {} parsed fully and unambiguously",
        counters.unambiguous_count
    );
    println!(
        "* {} parsed fully (but ambiguously)",
        counters.ambiguous_count
    );
    println!(
        "* {} parsed partially (only a prefix)",
        counters.too_short_count
    );
    println!(
        "* {} didn't parse at all (lexer error?)",
        counters.no_parse_count
    );
}

fn main() -> Result<(), failure::Error> {
    match Command::from_args() {
        Command::File {
            graphviz_forest,
            file,
        } => {
            // Not much to do, try to parse the file and report the result.
            parse_file_with(&file, |result| {
                let mut ambiguity_result = Ok(());
                match result {
                    Ok(handle) | Err(parse::ParseError::TooShort(handle)) => {
                        ambiguity_result = ambiguity_check(handle);

                        if let Some(out_path) = graphviz_forest {
                            handle
                                .parser
                                .sppf
                                .dump_graphviz(&mut fs::File::create(out_path).unwrap())
                                .unwrap();
                        }
                    }
                    Err(parse::ParseError::NoParse) => {}
                }
                report_file_result(None, result, ambiguity_result, None);
            });
        }
        Command::Dir { verbose, dir } => {
            let config = Config::load()?;

            // Find all the `.rs` files inside the desired directory.
            let files = WalkDir::new(dir)
                .contents_first(true)
                .into_iter()
                .map(|entry| entry.unwrap())
                .filter(|entry| entry.path().extension().map_or(false, |ext| ext == "rs"))
                .filter(|entry| !config.blacklist.is_blacklisted(entry.path()));

            // Go through all the files and try to parse each of them.

            let mut counters: Counters = files
                .par_bridge()
                .map(|f| process(f, verbose))
                .fold(
                    || Counters::default(),
                    |mut acc, x| {
                        acc.total_count += 1;
                        match x {
                            ParseResult::Ambiguous => {
                                acc.ambiguous_count += 1;
                            }
                            ParseResult::Unambiguous => {
                                acc.unambiguous_count += 1;
                            }
                            ParseResult::Partial => {
                                acc.too_short_count += 1;
                            }
                            ParseResult::Error => {
                                acc.no_parse_count += 1;
                            }
                        };
                        acc
                    },
                )
                .reduce(|| Counters::default(), |a, b| a + b);

            // We're done, time to print out stats!
            print_statistics(counters);
        }
    }
    Ok(())
}
