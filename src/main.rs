// Copyright (C) 2023 Stephane Raux. Distributed under the 0BSD license.

//! # Overview
//! - [⚖ 0BSD license](https://spdx.org/licenses/0BSD.html)
//!
//! CLI tool to clean Rust build artifacts.
//!
//! # Contribute
//! All contributions shall be licensed under the [0BSD license](https://spdx.org/licenses/0BSD.html).

#![deny(warnings)]

use clap::Parser;
use crossbeam::channel::Sender;
use indicatif::{ProgressBar, ProgressStyle};
use std::{
    fs::{read_dir, remove_dir_all, DirEntry},
    io,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::atomic::{self, AtomicUsize},
    thread,
};

/// Clean Rust build artifacts
#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Args {
    /// Number of concurrent jobs
    #[arg(long, default_value_t = 8)]
    concurrency: usize,
    /// Directory to scan recursively for build artifacts
    dir: PathBuf,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let status = Status::new();
    let success = clean_dir(&args.dir, args.concurrency, &status);
    status.indicator.finish();
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn clean_dir<O>(dir: &Path, worker_count: usize, observer: O) -> bool
where
    O: Observer + Sync,
{
    let (sender, receiver) = crossbeam::channel::unbounded();
    sender
        .send(Job::Scan(dir.to_owned(), sender.clone()))
        .unwrap();
    drop(sender);
    let has_error = thread::scope(|scope| {
        let workers = std::iter::repeat_with(|| {
            scope.spawn(|| {
                receiver.iter().fold(false, |has_error, job| {
                    let has_new_error = match job {
                        Job::Scan(dir, sender) => {
                            let has_error = match scan(&dir, sender.clone()) {
                                Ok(jobs) => jobs.fold(false, |has_error, job| match job {
                                    Ok(job) => sender.send(job).is_err() || has_error,
                                    Err(e) => {
                                        observer.on_error(e);
                                        true
                                    }
                                }),
                                Err(e) => {
                                    observer.on_error(e);
                                    true
                                }
                            };
                            observer.on_scanned(&dir);
                            has_error
                        }
                        Job::Remove(path) => match remove_dir_all(&path) {
                            Ok(()) => {
                                observer.on_removal(&path);
                                false
                            }
                            Err(e) => {
                                observer.on_error(e);
                                true
                            }
                        },
                    };
                    has_error || has_new_error
                })
            })
        })
        .take(worker_count)
        .collect::<Vec<_>>();
        workers.into_iter().any(|worker| worker.join().unwrap())
    });
    !has_error
}

trait Observer {
    fn on_error(&self, e: io::Error);
    fn on_removal(&self, path: &Path);
    fn on_scanned(&self, dir: &Path);
}

impl<T: Observer + ?Sized> Observer for &T {
    fn on_error(&self, e: io::Error) {
        (**self).on_error(e)
    }

    fn on_removal(&self, path: &Path) {
        (**self).on_removal(path)
    }

    fn on_scanned(&self, dir: &Path) {
        (**self).on_scanned(dir)
    }
}

struct Status {
    error_count: AtomicUsize,
    removed_count: AtomicUsize,
    scanned_count: AtomicUsize,
    indicator: ProgressBar,
}

impl Status {
    fn new() -> Self {
        Self {
            error_count: Default::default(),
            removed_count: Default::default(),
            scanned_count: Default::default(),
            indicator: ProgressBar::new_spinner()
                .with_style(ProgressStyle::with_template("{spinner} [{elapsed}] {msg}").unwrap()),
        }
    }

    fn update(&self) {
        let error_count = self.error_count.load(atomic::Ordering::SeqCst);
        let removed_count = self.removed_count.load(atomic::Ordering::SeqCst);
        let scanned_count = self.scanned_count.load(atomic::Ordering::SeqCst);
        self.indicator.set_message(format!(
            "{scanned_count} scanned, {removed_count} removed, {error_count} errors"
        ));
    }
}

impl Observer for Status {
    fn on_error(&self, _: io::Error) {
        self.error_count.fetch_add(1, atomic::Ordering::SeqCst);
        self.update();
    }

    fn on_removal(&self, _: &Path) {
        self.removed_count.fetch_add(1, atomic::Ordering::SeqCst);
        self.update();
    }

    fn on_scanned(&self, _: &Path) {
        self.scanned_count.fetch_add(1, atomic::Ordering::SeqCst);
        self.update();
    }
}

#[derive(Debug)]
enum Job {
    Scan(PathBuf, Sender<Job>),
    Remove(PathBuf),
}

fn scan(
    dir: &Path,
    sender: Sender<Job>,
) -> Result<impl Iterator<Item = Result<Job, io::Error>>, io::Error> {
    let mut state = ScanState::Nothing;
    read_dir(dir).map(|entries| {
        entries.filter_map(move |entry| {
            entry
                .and_then(|entry| process_entry(&mut state, entry, &sender))
                .transpose()
        })
    })
}

fn process_entry(
    state: &mut ScanState,
    entry: DirEntry,
    sender: &Sender<Job>,
) -> Result<Option<Job>, io::Error> {
    let path = entry.path();
    let file_type = entry.file_type()?;
    Ok(match path.file_name() {
        Some(name) => {
            if name == "Cargo.toml" {
                match state {
                    ScanState::Nothing => {
                        *state = ScanState::FoundCargoToml;
                        None
                    }
                    ScanState::FoundCargoToml => None,
                    ScanState::FoundTarget(target) => {
                        let target = std::mem::take(target);
                        *state = ScanState::FoundCargoToml;
                        Some(Job::Remove(target))
                    }
                }
            } else if file_type.is_dir() && name == "target" {
                match state {
                    ScanState::Nothing => {
                        *state = ScanState::FoundTarget(path);
                        None
                    }
                    ScanState::FoundCargoToml => Some(Job::Remove(path)),
                    ScanState::FoundTarget(_) => None,
                }
            } else if file_type.is_dir() {
                Some(Job::Scan(path, sender.clone()))
            } else {
                None
            }
        }
        None => None,
    })
}

#[derive(Debug)]
enum ScanState {
    Nothing,
    FoundCargoToml,
    FoundTarget(PathBuf),
}
