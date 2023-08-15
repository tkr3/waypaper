pub mod config;

#[derive(Debug)]
pub enum AppEvent {
    ConfigChanged,
    OutputChanged,
}
