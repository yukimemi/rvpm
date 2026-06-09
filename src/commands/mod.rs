//! CLI subcommand handlers (#217 stage 3, split per-subcommand in #228).
//!
//! Every `run_*` entry point invoked by the dispatcher in `lib.rs::run_cli`
//! lives in a focused submodule here. Shared helpers and the clap `Cli` /
//! `Commands` definitions stay in `lib.rs` and are reached via `crate::`
//! (glob-imported below; each submodule pulls these in with `use super::*`).

use crate::config::parse_config;
use crate::git::Repo;
use crate::merge::*;
use crate::paths::*;
use crate::plugin_build::*;
use crate::tui::{PluginStatus, TuiState};
use crate::url::*;
use crate::*;
use anyhow::{Context, Result};
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use dialoguer::{FuzzySelect, Select};
use ratatui::backend::CrosstermBackend;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use toml_edit::{DocumentMut, Item, table, value};

mod add;
mod browse;
mod clean;
mod completion;
mod config;
mod doctor;
mod edit;
mod generate;
mod init;
mod list;
mod log;
mod profile;
mod remove;
mod set;
mod sync;
mod tune;
mod update;

pub(crate) use add::*;
pub(crate) use browse::*;
pub(crate) use clean::*;
pub(crate) use completion::*;
pub(crate) use config::*;
pub(crate) use doctor::*;
pub(crate) use edit::*;
pub(crate) use generate::*;
pub(crate) use init::*;
pub(crate) use list::*;
pub(crate) use log::*;
pub(crate) use profile::*;
pub(crate) use remove::*;
pub(crate) use set::*;
pub(crate) use sync::*;
pub(crate) use tune::*;
pub(crate) use update::*;
