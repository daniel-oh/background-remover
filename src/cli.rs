//! The command-line mode: photos in, cutouts out, no server.
//!
//! `background-remover photo.jpg` writes `photo-cutout.png` beside it; `-`
//! reads stdin and writes stdout; several files run through one loaded
//! model. Everything else the service does (format, mask, parity) is the
//! same code.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::config::Config;
use crate::fetch::fetch_model;
use crate::imageops::{cutout_with, Options, Output};
use crate::model::{verify_checksum, Model};

/// What the arguments asked for.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// No files: run the HTTP service.
    Serve,
    /// Cut out these inputs.
    Cutout(Job),
    /// Download the model into the cache and print where it went: the
    /// named one, or the configured one.
    FetchModel(Option<String>),
    /// The container healthcheck.
    Health,
    /// Print the version.
    Version,
    /// Print the help text.
    Help,
}

/// A batch of inputs and how to write them.
#[derive(Debug, Default, PartialEq)]
pub struct Job {
    /// Input paths; `-` is stdin.
    pub inputs: Vec<PathBuf>,
    /// Explicit output path (one input only); `-` is stdout.
    pub output: Option<PathBuf>,
    /// Directory for the outputs, instead of beside each input.
    pub out_dir: Option<PathBuf>,
    /// PNG or WebP; `None` means decide from the output name, else PNG.
    pub format: Option<Output>,
    /// Write the mask alone.
    pub mask: bool,
    /// Model path from the command line.
    pub model: Option<PathBuf>,
    /// A known model named on the command line, instead of a path.
    pub model_name: Option<String>,
    /// Refuse to download a missing model.
    pub no_download: bool,
    /// ONNX Runtime threads; `None` means up to eight cores.
    pub threads: Option<usize>,
    /// No progress lines on stderr.
    pub quiet: bool,
    /// The subject on this colour instead of on nothing.
    pub background: Option<[u8; 3]>,
    /// Clean the edges' colour: `--refine`, `--no-refine`, or by default
    /// only with a background.
    pub refine: Option<bool>,
    /// Trim the output to the subject.
    pub crop: bool,
    /// Leave the picture as stored, ignoring its orientation tag.
    pub no_orient: bool,
    /// Replace an output file that already exists.
    pub force: bool,
}

/// Parse the arguments after the program name. Errors are usage errors.
pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut job = Job::default();
    // `--name=value` is `--name value`; everything after `--` is a file,
    // whatever it starts with.
    let mut words: Vec<String> = Vec::with_capacity(args.len());
    let mut files_only: Vec<PathBuf> = Vec::new();
    let mut options_over = false;
    for a in args {
        if options_over {
            files_only.push(PathBuf::from(a));
        } else if a == "--" {
            options_over = true;
        } else if let Some((name, v)) = a.split_once('=').filter(|_| a.starts_with("--")) {
            words.push(name.to_string());
            words.push(v.to_string());
        } else {
            words.push(a.clone());
        }
    }
    let mut fetch = false;
    let mut it = words.iter();
    let value = |flag: &str, next: Option<&String>| -> Result<String, String> {
        next.cloned().ok_or_else(|| format!("{flag} needs a value"))
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--health" => return Ok(Command::Health),
            "--version" | "-V" => return Ok(Command::Version),
            "--help" | "-h" => return Ok(Command::Help),
            "--fetch-model" => fetch = true,
            "-o" | "--output" => job.output = Some(PathBuf::from(value(a, it.next())?)),
            "-d" | "--out-dir" => job.out_dir = Some(PathBuf::from(value(a, it.next())?)),
            "-f" | "--format" => {
                job.format = Some(match value(a, it.next())?.to_ascii_lowercase().as_str() {
                    "png" => Output::Png,
                    "webp" => Output::Webp,
                    other => return Err(format!("unknown format {other}; png or webp")),
                })
            }
            "--mask" => job.mask = true,
            "--model" => {
                // A name this program knows, or else a file.
                let v = value(a, it.next())?;
                if crate::profile::known(&v).is_some() {
                    job.model_name = Some(v);
                } else {
                    job.model = Some(PathBuf::from(v));
                }
            }
            "--no-download" => job.no_download = true,
            "-j" | "--threads" => {
                job.threads = Some(
                    value(a, it.next())?
                        .parse()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or("--threads needs a positive number")?,
                )
            }
            "-q" | "--quiet" => job.quiet = true,
            "--bg" => {
                let text = value(a, it.next())?;
                job.background = Some(
                    crate::imageops::colour_of(&text)
                        .ok_or(format!("--bg {text}: a colour as RRGGBB, like ffffff"))?,
                )
            }
            "--refine" => job.refine = Some(true),
            "--no-refine" => job.refine = Some(false),
            "--crop" => job.crop = true,
            "--no-orient" => job.no_orient = true,
            "--force" => job.force = true,
            "-" => job.inputs.push(PathBuf::from("-")),
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            s => job.inputs.push(PathBuf::from(s)),
        }
    }
    job.inputs.extend(files_only);
    if fetch {
        return Ok(Command::FetchModel(job.model_name));
    }
    if job.inputs.is_empty() {
        if job != Job::default() {
            return Err("options given but no input files".into());
        }
        return Ok(Command::Serve);
    }
    if job.output.is_some() && job.inputs.len() > 1 {
        return Err("--output takes one input; use --out-dir for several".into());
    }
    if job.output.is_some() && job.out_dir.is_some() {
        return Err("--output and --out-dir together make no sense".into());
    }
    Ok(Command::Cutout(job))
}

/// Which output a job writes for a given output path.
pub fn output_kind(job: &Job, path: Option<&Path>) -> Output {
    if job.mask {
        return Output::MaskPng;
    }
    if let Some(f) = job.format {
        return f;
    }
    let webp = path
        .and_then(|p| p.extension())
        .is_some_and(|e| e.eq_ignore_ascii_case("webp"));
    if webp {
        Output::Webp
    } else {
        Output::Png
    }
}

/// Where the result of `input` goes: `-o` if given, else stdout for stdin,
/// else `NAME-cutout.EXT` (or `NAME-mask.png`) beside the input or in the
/// output directory.
pub fn output_path(job: &Job, input: &Path) -> PathBuf {
    if let Some(o) = &job.output {
        return o.clone();
    }
    if input == Path::new("-") {
        return PathBuf::from("-");
    }
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cutout".into());
    let name = match output_kind(job, None) {
        Output::MaskPng => format!("{stem}-mask.png"),
        Output::Webp => format!("{stem}-cutout.webp"),
        Output::Png => format!("{stem}-cutout.png"),
    };
    match &job.out_dir {
        Some(d) => d.join(name),
        None => input.with_file_name(name),
    }
}

/// Find the model, fetching it when allowed, and check it. Returns the path.
pub fn ensure_model(
    job: &Job,
    cfg: &Config,
    log: &mut dyn FnMut(String),
) -> Result<PathBuf, String> {
    let path = match &job.model {
        Some(p) => p.clone(),
        None => PathBuf::from(&cfg.model_path),
    };
    if !path.exists() {
        if job.no_download || job.model.is_some() || cfg.model_path_set {
            return Err(format!(
                "no model at {}\n  run `background-remover --fetch-model`, or point --model at the file",
                path.display()
            ));
        }
        let url = cfg.model_url.clone();
        let size = crate::profile::known(&cfg.model).map_or(String::new(), |k| {
            format!("{}, {} MB, ", k.name, k.megabytes)
        });
        log(format!(
            "fetching the model ({size}once) into {}",
            path.display()
        ));
        let mut shown = 0;
        fetch_model(&url, &path, &cfg.model_sha256, |done, total| {
            if let Some(total) = total {
                let pct = (done * 100 / total.max(1)) as u32;
                if pct >= shown + 10 || done == total {
                    shown = pct - pct % 10;
                    log(format!("  {pct}%"));
                }
            }
        })?;
    }
    verify_checksum(&path.to_string_lossy(), &cfg.model_sha256)?;
    Ok(path)
}

/// Run a job. Returns the exit code: 0 when every input succeeded, 1 when
/// any failed (the rest still ran).
pub fn run(job: &Job, cfg: &Config) -> i32 {
    // `--model NAME` swaps in that model's file, checksum and profile.
    let named;
    let cfg = match job.model_name.as_deref().and_then(crate::profile::known) {
        Some(known) => {
            named = cfg.with_model(known);
            &named
        }
        None => cfg,
    };
    let mut log = |line: String| {
        if !job.quiet {
            eprintln!("{line}");
        }
    };
    let model_path = match ensure_model(job, cfg, &mut log) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    // Up to eight cores: past that the efficiency cores on a laptop slow the
    // model down (an M1 Pro: 1.3 s a photo at 8, 2.2 s at all 10).
    let threads = job.threads.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get().min(8))
            .unwrap_or(2)
    });
    let model = Model::new(Config {
        model_path: model_path.to_string_lossy().into_owned(),
        threads,
        ..cfg.clone()
    })
    .quiet(job.quiet);

    let mut failed = 0;
    for input in &job.inputs {
        let started = Instant::now();
        let out = output_path(job, input);
        match one(job, &model, cfg, input, &out) {
            Ok(bytes) => log(format!(
                "{} -> {} ({} bytes, {:.1}s)",
                show(input),
                show(&out),
                bytes,
                started.elapsed().as_secs_f32()
            )),
            Err(e) => {
                failed += 1;
                eprintln!("{}: {e}", show(input));
            }
        }
    }
    if failed > 0 {
        1
    } else {
        0
    }
}

fn one(job: &Job, model: &Model, cfg: &Config, input: &Path, out: &Path) -> Result<usize, String> {
    let bytes = if input == Path::new("-") {
        let mut v = Vec::new();
        std::io::stdin()
            .read_to_end(&mut v)
            .map_err(|e| format!("cannot read stdin: {e}"))?;
        v
    } else {
        std::fs::read(input).map_err(|e| format!("cannot read: {e}"))?
    };
    if bytes.is_empty() {
        return Err("empty input".into());
    }
    // Checked before the work, not after it: a second of model time is a
    // poor way to find out the answer was going to be no.
    if out != Path::new("-") && !job.force && out.exists() {
        return Err(format!(
            "{} already exists; --force to replace it",
            out.display()
        ));
    }
    let opts = Options {
        output: output_kind(job, Some(out)),
        png_fast: cfg.png_fast,
        // Your own files on your own machine: no limit unless MAX_PIXELS sets one.
        max_pixels: cfg.max_pixels.unwrap_or(0),
        upright: !job.no_orient,
        refine: job.refine,
        background: job.background,
        crop: job.crop,
        model: None,
    };
    let result = cutout_with(&bytes, model, &opts)
        .map_err(|e| e.to_string())?
        .bytes;
    if out == Path::new("-") {
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&result)
            .and_then(|_| stdout.flush())
            .map_err(|e| format!("cannot write stdout: {e}"))?;
    } else {
        if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        std::fs::write(out, &result).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
    }
    Ok(result.len())
}

fn show(p: &Path) -> String {
    if p == Path::new("-") {
        "stdin/stdout".into()
    } else {
        p.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn no_arguments_means_serve() {
        assert_eq!(parse(&[]).unwrap(), Command::Serve);
        assert_eq!(parse(&args("--health")).unwrap(), Command::Health);
        assert_eq!(
            parse(&args("--fetch-model")).unwrap(),
            Command::FetchModel(None)
        );
        assert_eq!(
            parse(&args("--model birefnet-general-lite --fetch-model")).unwrap(),
            Command::FetchModel(Some("birefnet-general-lite".into()))
        );
    }

    #[test]
    fn files_and_options_make_a_job() {
        let Command::Cutout(job) = parse(&args("a.jpg b.png -d out -f webp -j 4 -q")).unwrap()
        else {
            panic!()
        };
        assert_eq!(job.inputs.len(), 2);
        assert_eq!(job.out_dir.as_deref(), Some(Path::new("out")));
        assert_eq!(job.format, Some(Output::Webp));
        assert_eq!(job.threads, Some(4));
        assert!(job.quiet);
    }

    #[test]
    fn a_model_is_a_known_name_or_else_a_file() {
        let Command::Cutout(job) = parse(&args("a.jpg --model birefnet-general")).unwrap() else {
            panic!()
        };
        assert_eq!(job.model_name.as_deref(), Some("birefnet-general"));
        assert_eq!(job.model, None);
        let Command::Cutout(job) = parse(&args("a.jpg --model weights/mine.onnx")).unwrap() else {
            panic!()
        };
        assert_eq!(job.model_name, None);
        assert_eq!(job.model, Some(PathBuf::from("weights/mine.onnx")));
    }

    #[test]
    fn usage_errors_are_errors() {
        assert!(parse(&args("--bogus")).is_err());
        assert!(parse(&args("-o x.png")).is_err());
        assert!(parse(&args("a.jpg b.jpg -o x.png")).is_err());
        assert!(parse(&args("a.jpg -o x.png -d out")).is_err());
        assert!(parse(&args("a.jpg -f gif")).is_err());
        assert!(parse(&args("a.jpg -j 0")).is_err());
        assert!(parse(&args("a.jpg -o")).is_err());
        assert!(parse(&args("a.jpg --bg notacolour")).is_err());
    }

    #[test]
    fn long_options_take_an_equals_and_two_dashes_end_them() {
        let Command::Cutout(job) = parse(&args(
            "--format=webp --bg=#fff --crop --refine --force -- -odd.jpg --mask",
        ))
        .unwrap() else {
            panic!()
        };
        assert_eq!(job.format, Some(Output::Webp));
        assert_eq!(job.background, Some([255, 255, 255]));
        assert!(job.crop && job.force && !job.mask);
        assert_eq!(job.refine, Some(true));
        assert_eq!(
            job.inputs,
            vec![PathBuf::from("-odd.jpg"), PathBuf::from("--mask")]
        );
    }

    #[test]
    fn output_names_follow_the_input_and_the_kind() {
        let job = Job::default();
        assert_eq!(
            output_path(&job, Path::new("pics/photo.jpg")),
            PathBuf::from("pics/photo-cutout.png")
        );
        let webp = Job {
            format: Some(Output::Webp),
            out_dir: Some(PathBuf::from("out")),
            ..Job::default()
        };
        assert_eq!(
            output_path(&webp, Path::new("photo.jpg")),
            PathBuf::from("out/photo-cutout.webp")
        );
        let mask = Job {
            mask: true,
            ..Job::default()
        };
        assert_eq!(
            output_path(&mask, Path::new("photo.jpg")),
            PathBuf::from("photo-mask.png")
        );
        assert_eq!(output_path(&job, Path::new("-")), PathBuf::from("-"));
        let explicit = Job {
            output: Some(PathBuf::from("x.webp")),
            ..Job::default()
        };
        assert_eq!(
            output_kind(&explicit, Some(Path::new("x.webp"))),
            Output::Webp
        );
        assert_eq!(
            output_kind(&mask, Some(Path::new("x.webp"))),
            Output::MaskPng
        );
    }
}
