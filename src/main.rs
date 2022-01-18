#![forbid(unsafe_code)]
#![feature(option_result_contains)]
#![feature(hash_drain_filter)]
// This will only work on linux, we're using DMenu anyways.
#![cfg(target_os = "linux")]
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::{
    fs::{DirEntry, File},
    io::BufWriter,
    os::unix::prelude::PermissionsExt,
    path::PathBuf,
};

use fork::{daemon, Fork};
use itertools::Itertools;

lazy_static::lazy_static! {
    pub static ref DESKTOP_FOLDER: PathBuf = dirs::home_dir().unwrap().join("Desktop");
    pub static ref LOCAL_APPLICATIONS: PathBuf = dirs::data_local_dir().unwrap().join("applications");
    pub static ref DESKTOP_DIRS: [PathBuf; 3] = [
        DESKTOP_FOLDER.to_path_buf(),
        PathBuf::from("/usr/share/applications"),
        LOCAL_APPLICATIONS.to_path_buf(),
    ];
    pub static ref PATH: String = std::env::var("PATH").unwrap_or_default();
    pub static ref PATH_DIRS: Vec<PathBuf> = PATH
        .split(':')
        .filter_map(|x| PathBuf::try_from(x).ok())
        .collect();
}

type Result<T = ()> = core::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> Result {
    let args = std::env::args().collect_vec();

    if args.contains(&"--help".to_string()) {
        println!("Usage: dmenu_drun [--help] [-d] [-p]");
        println!("    -p        hide files in $PATH");
        println!("    -d        hide desktop files");
        return Ok(());
    }

    let cache_dir = dirs::cache_dir().unwrap();
    std::fs::create_dir_all(&cache_dir)?;
    let cache_path = cache_dir.join(".dmenu_rs_cache");

    let cache_mtime = cache_path
        .metadata()
        .map_or_else(|_| std::time::UNIX_EPOCH, |x| x.modified().unwrap());

    let rebuild_cache = !cache_path.exists()
        || PATH_DIRS.iter().chain(DESKTOP_DIRS.iter()).any(|x| {
            x.metadata()
                .map(|x| x.modified().unwrap() > cache_mtime)
                .unwrap_or(false)
        });

    let mut cache_file = File::options()
        .read(true)
        .write(rebuild_cache)
        .append(!rebuild_cache)
        .open(&cache_path)
        .or_else(|_| File::create(&cache_path))
        .expect("Could not create cache file");

    let mut cache = if rebuild_cache {
        let mut cache = create_path_cache(&cache_file)?.0;
        cache.extend(create_desktop_cache(&cache_file)?.0);
        cache
    } else {
        let mut cache_str = String::new();
        cache_file.read_to_string(&mut cache_str)?;
        Cache::from_str(&cache_str)?.0
    };

    if args.contains(&"-p".to_string()) {
        cache = cache.drain_filter(|k, v| k != v).collect();
    }

    if args.contains(&"-d".to_string()) {
        cache = cache
            .drain_filter(|_, v| !v.ends_with(".desktop"))
            .collect();
    }

    let histfile =
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".dmenu_drun_histfile");

    let dmenu = Command::new("dmenu")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .args(["-H", histfile.to_string_lossy().to_string().as_str()])
        .spawn()
        .expect("Could not spawn dmenu");

    let mut dmenu_stdin = dmenu.stdin.as_ref().expect("Could not write to dmenu");

    let mut formatted = cache.keys().collect_vec();
    formatted.sort_unstable();
    formatted.dedup();
    let formatted = formatted.iter().join("\n");

    writeln!(dmenu_stdin, "{}", formatted)?;

    let result = dmenu.wait_with_output().expect("Could not wait for dmenu");
    let output = String::from_utf8_lossy(&result.stdout)
        .trim()
        .trim_end_matches(".desktop")
        .to_string();

    let entry = cache.get(&output);
    if let Some(entry) = entry {
        if &output == entry {
            let _ = Command::new(entry)
                .spawn()
                .expect("Could not start target executable")
                .wait();
        } else {
            // Gtk-launch spawns a child process, needs double-fork
            if let Ok(Fork::Child) = daemon(true, true) {
                let _ = Command::new("gtk-launch")
                    .arg(entry)
                    .spawn()
                    .expect("Could not start target executable")
                    .wait();
            }
        }
    } else {
        let mut output = output.split_whitespace();
        let _ = Command::new(output.next().expect("Got empty output from dmenu"))
            .args(output.collect_vec())
            .spawn()
            .expect("Could not start target executable")
            .wait();
    }
    std::process::exit(result.status.code().unwrap_or(-1));
}

#[derive(Clone, Debug, PartialEq, Default)]
struct Cache(HashMap<String, String>);

impl std::fmt::Display for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (k, v) in &self.0 {
            writeln!(f, "{}\0{}", k, v)?;
        }
        Ok(())
    }
}

impl FromStr for Cache {
    type Err = std::fmt::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self(
            s.lines()
                .filter_map(|x| {
                    x.split('\0')
                        .map(ToString::to_string)
                        .collect_tuple::<(_, _)>()
                })
                .collect(),
        ))
    }
}

fn create_cache<P: FnMut(&DirEntry) -> bool, L: FnMut(String, &File) -> String>(
    cache_file: &File,
    dirs: impl Iterator<Item = &'static PathBuf>,
    mut predicate: P,
    mut localizer: L,
) -> Result<Cache> {
    let mut writer = BufWriter::new(cache_file);
    let mut cache = Cache::default();
    for entry in dirs.read_dir_exists_filtered(|x| predicate(x)) {
        let file_path = entry.path();
        let file = File::open(&file_path);
        if let Ok(file) = file {
            let file_name = file_path
                .file_name()
                .ok_or(std::fmt::Error)?
                .to_string_lossy()
                .to_string();
            cache
                .0
                .insert(localizer(file_name.clone(), &file), file_name);
        }
    }
    write!(writer, "{}", cache)?;
    Ok(cache)
}

fn create_desktop_cache(cache_file: &File) -> Result<Cache> {
    create_cache(
        cache_file,
        DESKTOP_DIRS.iter(),
        |x| {
            if let Some(ext) = x.path().extension() {
                ext.to_string_lossy() == "desktop"
                    && x.metadata().map(|y| y.is_file()).unwrap_or_default()
            } else {
                false
            }
        },
        |_, file| {
            let bufreader = BufReader::new(file);
            bufreader
                .lines()
                .filter_map(|x| x.ok())
                .find(|x| x.starts_with("Name="))
                .unwrap_or_default()
                .trim_start_matches("Name=")
                .to_string()
        },
    )
}

fn create_path_cache(cache_file: &File) -> Result<Cache> {
    create_cache(
        cache_file,
        PATH_DIRS.iter(),
        |x| {
            x.metadata()
                .map(|meta| !meta.permissions().mode() & 0o111)
                .contains(&0)
                && x.metadata().map(|y| y.is_file()).unwrap_or_default()
        },
        |name, _| name,
    )
}

/// Trait used to return an `Iterator` over all `DirEntry`'s
/// that exist
trait ReadDirExists: Sized {
    /// Returns all `Direntry`'s in the directories in a
    /// given iterator that exists.
    /// i.e. filter everything out that does not exist.
    fn read_dir_exists(self) -> Vec<DirEntry> {
        self.read_dir_exists_filtered(|_| true)
    }
    /// See `read_dir_exists`. Applies a filter, before collecting.
    fn read_dir_exists_filtered<P: FnMut(&DirEntry) -> bool>(self, predicate: P) -> Vec<DirEntry>;
}

impl<I, T> ReadDirExists for I
where
    I: IntoIterator<Item = T>,
    T: Sized,
    PathBuf: From<T>,
{
    fn read_dir_exists_filtered<P: FnMut(&DirEntry) -> bool>(
        self,
        mut predicate: P,
    ) -> Vec<DirEntry> {
        self.into_iter()
            .filter_map(|x| {
                let path = PathBuf::from(x);
                std::fs::read_dir(path).ok()
            })
            .flat_map(|x| x.filter_map(|x| x.ok()))
            .filter(|x| predicate(x))
            .collect()
    }
}
