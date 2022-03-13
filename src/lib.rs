use anyhow::{bail, Context, Error, Result};
use chrono::{DateTime, Local};
use diff_utils::{Comparison, DisplayOptions, PatchOptions};
use regex::Regex;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::{collections::HashMap, io::Write};

fn assert_section(entry: Entry, actual: String) -> Result<()> {
    let mut new_snap_path: PathBuf = entry.entry.into();
    let ext = format!("{}.new", entry.section_name);
    new_snap_path.set_extension(&ext);

    let expected = format!("{}{}", entry.section, entry.last_line);

    if expected != actual {
        let expected_lines = expected.lines().collect::<Vec<_>>();
        let actual_lines = actual.lines().collect::<Vec<_>>();
        let comparison = Comparison::new(&expected_lines, &actual_lines).compare()?;
        eprintln!(
            "\nFound mismatch in section [{}] in {}\n{}",
            entry.section_name,
            entry.entry.display(),
            comparison.display(DisplayOptions {
                offset: entry.line,
                ..Default::default()
            })
        );

        std::fs::File::create(&new_snap_path).and_then(|mut file| {
            let datetime: DateTime<Local> = entry.modified.into();
            let dt = datetime.format("%F %T %z");

            let entry_basename = entry.entry.file_name().unwrap().to_string_lossy();
            let snap_basename = new_snap_path.file_name().unwrap().to_string_lossy();

            // writeln!(file, "```")?;
            // writeln!(file, "{}", entry.input)?;
            // writeln!(file, "```")?;
            write!(
                file,
                "{}",
                comparison.patch(
                    entry_basename,
                    &dt,
                    snap_basename,
                    &dt,
                    PatchOptions { offset: entry.line }
                )
            )
        })?;

        bail!("failed");
    } else if new_snap_path.exists() {
        std::fs::remove_file(new_snap_path)?;
    }

    Ok(())
}

#[derive(Debug)]
enum EntryKind {
    Input,
    Expected,
}

#[derive(Debug)]
struct Entry<'a> {
    kind: EntryKind,
    section_name: &'a str,
    line: usize,
    section: &'a str,
    entry: &'a Path,
    modified: SystemTime,
    last_line: &'a str,
}

pub struct SnapshotInputs {
    inputs: HashMap<String, String>,
}

impl SnapshotInputs {
    pub fn get_str(&self, key: &str) -> Result<&str> {
        let input = self
            .inputs
            .get(key)
            .with_context(|| format!("Could not find a snapshot section called: {}", key))?;
        Ok(&input)
    }
    pub fn get_json<T: DeserializeOwned>(&self, key: &str) -> Result<T> {
        let input = self
            .inputs
            .get(key)
            .with_context(|| format!("Could not find a snapshot section called: {}", key))?;
        serde_json::from_str(&input).map_err(Error::from)
    }
}

pub fn test_snapshots<F>(section_name: &'static str, f: F) -> Result<()>
where
    F: 'static + std::panic::RefUnwindSafe + Fn(&SnapshotInputs) -> String + Send,
{
    const TIMEOUT: u32 = 60_000;
    use pulse::{Signal, TimeoutError};
    let (signal_start, pulse_start) = Signal::new();
    let (signal_end, pulse_end) = Signal::new();

    let guard = std::thread::spawn(move || {
        pulse_start.pulse();
        let result = test_snapshots_inner(section_name, f);
        pulse_end.pulse();
        result
    });

    signal_start.wait().unwrap();
    match signal_end.wait_timeout_ms(TIMEOUT) {
        Err(TimeoutError::Timeout) => {
            bail!("Timed out");
        }
        _ => (),
    }

    guard.join().unwrap()
}

fn test_snapshots_inner<F>(section_name: &str, f: F) -> Result<()>
where
    F: std::panic::RefUnwindSafe + Fn(&SnapshotInputs) -> String,
{
    struct CurrentSection<'a> {
        from: usize,
        from_inner: usize,
        to: usize,
        last_line: Option<(usize, usize)>,
        line: usize,
        name: &'a str,
    }

    impl<'a> CurrentSection<'a> {
        fn into_entry(self, source: &'a str, entry: &'a PathBuf) -> Result<Entry<'a>> {
            let (from, kind) = if self.name.starts_with("expected.") {
                (self.from, EntryKind::Expected)
            } else {
                (self.from_inner, EntryKind::Input)
            };

            let metadata = std::fs::metadata(&entry)?;

            let last_line = match self.last_line {
                Some((from, to)) => &source[from..to],
                None => &source[self.to..self.to],
            };

            Ok(Entry {
                kind,
                entry,
                section_name: self.name,
                section: &source[from..self.to],
                line: self.line,
                last_line,
                modified: metadata.modified()?,
            })
        }
    }

    let section_regex = Regex::new(r"^\s*\[([[:alpha:]\.-_]+)\]\s*$")?;
    let path = std::env::current_dir()?;
    let mut successes = 0;
    let mut processed = 0;
    let mut skipped = 0;
    for entry in glob::glob(&format!("{}/tests/**/*.snap", path.display()))? {
        let entry = entry?;
        let entry_file = load_file(&entry)?;
        let mut sections: HashMap<String, Entry> = HashMap::default();
        let mut current_section: Option<CurrentSection> = None;
        let input_len = entry_file.lines().count();
        for (line_idx, line) in entry_file.lines().enumerate() {
            if let Some(captures) = section_regex.captures(line) {
                let offset = offset(&entry_file, line);
                let len = line.len();

                if let Some(mut current_section) = current_section.take() {
                    current_section.to = offset;
                    current_section.last_line = Some((offset, offset + len));
                    sections.insert(
                        current_section.name.into(),
                        current_section.into_entry(&entry_file, &entry)?,
                    );
                }
                let name = captures.get(1).unwrap().as_str();
                current_section = Some(CurrentSection {
                    name,
                    from: offset,
                    from_inner: offset + len,
                    to: offset + len,
                    last_line: None,
                    line: input_len + line_idx,
                });
            }
        }

        if let Some(mut current_section) = current_section.take() {
            current_section.to = entry_file.len();
            sections.insert(
                current_section.name.into(),
                current_section.into_entry(&entry_file, &entry)?,
            );
        }

        let section_name = format!("expected.{}", section_name);
        let (inputs, mut expected): (HashMap<_, _>, HashMap<_, _>) = sections
            .into_iter()
            .partition(|(_name, section)| matches!(section.kind, EntryKind::Input));

        if let Some(section) = expected.remove(&section_name) {
            let inputs = inputs
                .into_iter()
                .map(|(k, v)| (k, v.section.into()))
                .collect();
            let inputs = SnapshotInputs { inputs };
            let result = std::panic::catch_unwind(|| f(&inputs));
            match result {
                Ok(output) => {
                    let actual = format!("[{}]\n{}\n\n{}", section_name, output, section.last_line);
                    match assert_section(section, actual) {
                        Ok(_) => {
                            successes += 1;
                            eprint!(".");
                        }
                        Err(e) => {
                            eprintln!("{}: {:?}\n", entry.display(), e);
                        }
                    }
                }

                Err(_) => {
                    eprintln!("{}: Thread panicked\n", entry.display());
                }
            }
            processed += 1;
        } else {
            skipped += 1;
        }
    }
    eprintln!(
        "\nProcessed {}: {}, Failed: {}, Skipped: {}",
        section_name,
        processed,
        processed - successes,
        skipped
    );
    if successes != processed {
        bail!("Some tests failed");
    }
    Ok(())
}

fn offset(parent: &str, child: &str) -> usize {
    let parent_ptr = parent.as_ptr() as usize;
    let child_ptr = child.as_ptr() as usize;
    child_ptr - parent_ptr
}

fn load_file(entry: &Path) -> Result<String> {
    let s = std::fs::read_to_string(entry)?;
    Ok(s)
}
