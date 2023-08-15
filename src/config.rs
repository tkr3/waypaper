use std::{
    collections::HashMap,
    error::Error,
    fmt::Display,
    path::{Path, PathBuf},
    str::FromStr,
};

use log::{debug, error, info, warn};
use notify::Watcher;

use crate::AppEvent;

const CONFIG_FILE_NAME: &str = "waypaper.ini";
const CONFIG_DIR_NAME: &str = "waypaper";

#[derive(Debug, Default)]
pub struct Config {
    pub config_path: Option<PathBuf>,
    pub output_preferences: Option<HashMap<String, OutputPreferences>>,
}

impl Config {
    pub fn search() -> Config {
        let config_file_path = Self::search_config_file();

        if let Ok(path) = config_file_path {
            info!("Config file found at: {}", path.display());
            assert!(path.exists());
            assert!(path.is_file());

            return Config::new(path);
        } else {
            warn!("No config file found, using default config");
            return Config::default();
        }
    }

    fn new(config_path: PathBuf) -> Config {
        info!("Loading config file");

        let output_preferences = parse_config(
            ini::Ini::load_from_file(config_path.clone()).unwrap_or_else(|e| {
                error!("Error while loading config file: {}", e);
                warn!("Using empty config");
                ini::Ini::new()
            }),
        );

        return Config {
            config_path: Some(config_path),
            output_preferences: Some(output_preferences),
        };
    }

    pub fn watch(
        &self,
    ) -> (
        notify::RecommendedWatcher,
        std::sync::mpsc::Receiver<AppEvent>,
        std::sync::mpsc::Sender<AppEvent>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();

        let sender = tx.clone();
        let mut watcher = notify::recommended_watcher(
            move |res: Result<notify::Event, notify::Error>| match res {
                Ok(event) => {
                    if event.kind
                        == notify::EventKind::Access(notify::event::AccessKind::Close(
                            notify::event::AccessMode::Write,
                        ))
                    {
                        debug!("Config file changed");
                        sender.send(AppEvent::ConfigChanged).unwrap();
                    }
                }
                Err(e) => {
                    error!("Notify error: {}", e);
                }
            },
        )
        .unwrap();

        info!("Watching config file for changes");

        watcher
            .watch(
                self.config_path.as_ref().unwrap(),
                notify::RecursiveMode::NonRecursive,
            )
            .unwrap();

        (watcher, rx, tx)
    }

    pub fn reload(&mut self) -> Result<(), Box<dyn Error>> {
        info!("Reloading config file");

        let config = if let Some(config_path) = &self.config_path {
            ini::Ini::load_from_file(config_path.clone())?
        } else {
            return Err("Config file not found".into());
        };

        let output_preferences = parse_config(config);

        self.output_preferences.replace(output_preferences);

        Ok(())
    }

    fn search_config_file() -> Result<PathBuf, ()> {
        // Try to find the config file in the following locations:
        // 1. The current working directory
        // 2. The user's .config directory
        // 3. The global /etc/ directory

        // If the config file is found in either of these locations, return the path to the file
        // If the config file is not found in either of these locations, return an error

        info!("Searching for config file");

        info!("Searching in current working directory");

        if let Ok(cwd) = std::env::current_dir() {
            let config_file_path = cwd.join(CONFIG_FILE_NAME);
            if config_file_path.exists() {
                return Ok(config_file_path);
            }
        }

        info!("Searching in user's config directory");

        let home_config_path = dirs::config_dir()
            .unwrap()
            .join(CONFIG_DIR_NAME)
            .join(CONFIG_FILE_NAME);

        if home_config_path.exists() {
            return Ok(home_config_path);
        }

        info!("Searching in global config directory");

        let global_config_path = Path::new("/etc")
            .join(CONFIG_DIR_NAME)
            .join(CONFIG_FILE_NAME);

        if global_config_path.exists() {
            return Ok(global_config_path);
        }

        Err(())
    }
}

fn parse_config(config: ini::Ini) -> HashMap<String, OutputPreferences> {
    let mut output_preferences = HashMap::new();

    config
        .sections()
        .filter(Option::is_some)
        .for_each(|section_name| {
            let section = config.section(section_name);

            let output_name = section_name.unwrap().to_string();

            let background = section.and_then(|section| {
                section
                    .get("background")
                    .map(|background| Path::new(background).to_path_buf())
            });

            let mode = section
                .and_then(|section| section.get("mode").map(|mode| Mode::from_str(mode).ok()))
                .flatten()
                .unwrap_or_default();

            output_preferences.insert(output_name, OutputPreferences { background, mode });
        });
    output_preferences
}

#[derive(Debug)]
pub struct OutputPreferences {
    pub background: Option<PathBuf>,
    pub mode: Mode,
}

#[derive(Debug)]
pub enum Mode {
    Center,
    Fill,
    Fit,
    Stretch,
}

impl Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Mode::Center => "center",
                Mode::Fill => "fill",
                Mode::Fit => "fit",
                Mode::Stretch => "stretch",
            }
        )
    }
}

impl Default for Mode {
    fn default() -> Self {
        Mode::Fill
    }
}

impl FromStr for Mode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "center" => Ok(Mode::Center),
            "fill" => Ok(Mode::Fill),
            "fit" => Ok(Mode::Fit),
            "stretch" => Ok(Mode::Stretch),
            _ => Err(()),
        }
    }
}
