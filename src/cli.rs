use std::cmp::max;
use std::collections::HashMap;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::process;
use std::rc::Rc;

use crate::config_compat::SerdeReplace;
use log::{LevelFilter, error};
use nanoargs::{ArgBuilder, ArgParser, Flag, Opt, ParseError};
use serde::{Deserialize, Serialize};
use toml::Value;

use crate::terminal::tty::Options as PtyOptions;

use crate::config::UiConfig;
use crate::config::ui_config::Program;
use crate::config::window::{Class, Identity};
use crate::logging::LOG_TARGET_IPC_CONFIG;

/// CLI options for the main Alacritty executable.
#[derive(Default, Debug)]
pub struct Options {
    /// Print all events to STDOUT.
    pub print_events: bool,

    /// Generates ref test.
    #[cfg(feature = "ref-tests")]
    pub ref_test: bool,

    /// Window ID to embed Alacritty within (decimal or hexadecimal with "0x" prefix).
    #[cfg(all(unix, not(target_os = "macos")))]
    pub embed: Option<String>,

    /// Specify alternative configuration file [default: %APPDATA%\alacritty\alacritty.toml].
    pub config_file: Option<PathBuf>,

    /// Reduces the level of verbosity (the min level is -qq).
    quiet: u8,

    /// Increases the level of verbosity (the max level is -vvv).
    verbose: u8,

    /// Do not spawn an initial window.
    #[cfg(unix)]
    pub daemon: bool,

    /// IPC socket path.
    #[cfg(unix)]
    pub socket: Option<PathBuf>,

    /// CLI options for config overrides.
    pub config_options: ParsedOptions,

    /// Options which can be passed when creating a new window.
    pub window_options: WindowOptions,
}

impl Options {
    pub fn new() -> Self {
        let args = std::env::args_os()
            .skip(1)
            .map(|arg| {
                arg.into_string()
                    .map_err(|arg| CliError::Value(arg.to_string_lossy().into_owned()))
            })
            .collect::<Result<Vec<_>, _>>();

        match args.and_then(Self::parse) {
            Ok(options) => options,
            Err(CliError::Parse(ParseError::HelpRequested(help))) => {
                println!("{help}");
                process::exit(0);
            }
            Err(CliError::Parse(ParseError::VersionRequested(version))) => {
                println!("{version}");
                process::exit(0);
            }
            Err(err) => {
                eprintln!("error: {err}");
                process::exit(2);
            }
        }
    }

    fn parser() -> ArgParser {
        let parser = ArgBuilder::new()
            .name("alacritty")
            .description("A fast, cross-platform terminal emulator")
            .version(env!("VERSION"))
            .flag(Flag::new("print-events").desc("Print all events to stdout"))
            .flag(Flag::new("quiet").short('q').desc("Reduce log verbosity"))
            .flag(
                Flag::new("verbose")
                    .short('v')
                    .desc("Increase log verbosity"),
            )
            .option(
                Opt::new("config-file")
                    .placeholder("PATH")
                    .desc("Use an alternative configuration file"),
            )
            .option(
                Opt::new("working-directory")
                    .placeholder("PATH")
                    .desc("Start the shell in this directory"),
            )
            .flag(Flag::new("hold").desc("Remain open after child process exit"))
            .option(
                Opt::new("command")
                    .short('e')
                    .placeholder("COMMAND ...")
                    .desc("Execute command with arguments; must be last"),
            )
            .option(
                Opt::new("title")
                    .short('T')
                    .placeholder("TITLE")
                    .desc("Define the window title"),
            )
            .option(
                Opt::new("class")
                    .placeholder("GENERAL[,INSTANCE]")
                    .desc("Define the window class"),
            )
            .option(
                Opt::new("option")
                    .short('o')
                    .placeholder("KEY=VALUE")
                    .desc("Override configuration options")
                    .multi(),
            )
            .conflict("verbosity", &["quiet", "verbose"]);

        #[cfg(feature = "ref-tests")]
        let parser = parser.flag(Flag::new("ref-test").desc("Generate a reference test"));
        #[cfg(all(unix, not(target_os = "macos")))]
        let parser = parser.option(
            Opt::new("embed")
                .placeholder("WINDOW_ID")
                .desc("Embed into an existing window"),
        );
        #[cfg(unix)]
        let parser = parser
            .flag(Flag::new("daemon").desc("Do not spawn an initial window"))
            .option(
                Opt::new("socket")
                    .placeholder("PATH")
                    .desc("Use a custom IPC socket path"),
            );
        #[cfg(all(unix, feature = "ref-tests"))]
        let parser = parser.conflict("runtime mode", &["ref-test", "daemon"]);

        parser.build().expect("valid CLI schema")
    }

    fn parse(mut args: Vec<String>) -> Result<Self, CliError> {
        let command = split_command(&mut args)?;
        normalize_title_alias(&mut args);
        normalize_config_options(&mut args);
        let quiet = count_verbosity(&args, 'q', "--quiet");
        let verbose = count_verbosity(&args, 'v', "--verbose");
        let parsed = Self::parser().parse(args).map_err(CliError::Parse)?;

        let terminal_options = TerminalOptions {
            working_directory: parsed.get_option("working-directory").map(PathBuf::from),
            hold: parsed.get_flag("hold"),
            command,
        };
        let window_identity = WindowIdentity {
            title: parsed.get_option("title").map(ToOwned::to_owned),
            class: parsed
                .get_option("class")
                .map(parse_class)
                .transpose()
                .map_err(|err| CliError::Value(format!("invalid value for --class: {err}")))?,
        };
        let window_options = WindowOptions {
            terminal_options,
            window_identity,
            #[cfg(target_os = "macos")]
            window_tabbing_id: None,
            option: parsed.get_option_values("option").to_vec(),
        };
        let config_options = window_options.config_overrides();

        Ok(Self {
            print_events: parsed.get_flag("print-events"),
            #[cfg(feature = "ref-tests")]
            ref_test: parsed.get_flag("ref-test"),
            #[cfg(all(unix, not(target_os = "macos")))]
            embed: parsed.get_option("embed").map(ToOwned::to_owned),
            config_file: parsed.get_option("config-file").map(PathBuf::from),
            quiet,
            verbose,
            #[cfg(unix)]
            daemon: parsed.get_flag("daemon"),
            #[cfg(unix)]
            socket: parsed.get_option("socket").map(PathBuf::from),
            config_options,
            window_options,
        })
    }

    /// Override configuration file with options from the CLI.
    pub fn override_config(&mut self, config: &mut UiConfig) {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            config.window.embed = self
                .embed
                .as_ref()
                .and_then(|embed| parse_hex_or_decimal(embed));
        }

        config.debug.print_events |= self.print_events;
        config.debug.log_level = max(config.debug.log_level, self.log_level());
        #[cfg(feature = "ref-tests")]
        {
            config.debug.ref_test |= self.ref_test;
        }

        if config.debug.print_events {
            config.debug.log_level = max(config.debug.log_level, LevelFilter::Info);
        }

        self.config_options.override_config(config);
    }

    pub fn daemon(&self) -> bool {
        #[cfg(unix)]
        {
            self.daemon
        }

        #[cfg(not(unix))]
        {
            false
        }
    }

    /// Logging filter level.
    pub fn log_level(&self) -> LevelFilter {
        match (self.quiet, self.verbose) {
            // Force at least `Info` level for `--print-events`.
            (_, 0) if self.print_events => LevelFilter::Info,

            // Default.
            (0, 0) => LevelFilter::Warn,

            // Verbose.
            (_, 1) => LevelFilter::Info,
            (_, 2) => LevelFilter::Debug,
            (0, _) => LevelFilter::Trace,

            // Quiet.
            (1, _) => LevelFilter::Error,
            (..) => LevelFilter::Off,
        }
    }
}

#[derive(Debug)]
enum CliError {
    Parse(ParseError),
    Value(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(err) => err.fmt(formatter),
            Self::Value(err) => formatter.write_str(err),
        }
    }
}

/// Remove the command tail before parsing, since every token after `-e` belongs to the child.
fn split_command(args: &mut Vec<String>) -> Result<Vec<String>, CliError> {
    let mut after_separator = false;
    for index in 0..args.len() {
        let token = &args[index];
        if token == "--" {
            after_separator = true;
            continue;
        }
        if after_separator {
            continue;
        }

        let attached = token
            .strip_prefix("--command=")
            .or_else(|| token.strip_prefix("-e="))
            .or_else(|| token.strip_prefix("-e").filter(|value| !value.is_empty()));
        if token != "-e" && token != "--command" && attached.is_none() {
            continue;
        }

        let mut tail = args.split_off(index);
        let option = tail.remove(0);
        let mut command = Vec::new();
        if let Some(attached) = option
            .strip_prefix("--command=")
            .or_else(|| option.strip_prefix("-e="))
            .or_else(|| option.strip_prefix("-e").filter(|value| !value.is_empty()))
        {
            command.push(attached.to_owned());
        }
        command.extend(tail);

        if command.is_empty() {
            return Err(CliError::Value(format!(
                "option {option} requires a command"
            )));
        }
        return Ok(command);
    }

    Ok(Vec::new())
}

/// Nanoargs has no aliases, so normalize the historical `-t` title alias.
fn normalize_title_alias(args: &mut [String]) {
    for arg in args {
        if arg == "-t" {
            *arg = String::from("-T");
        } else if let Some(title) = arg.strip_prefix("-t=") {
            *arg = format!("-T={title}");
        } else if let Some(title) = arg.strip_prefix("-t").filter(|title| !title.is_empty()) {
            *arg = format!("-T{title}");
        }
    }
}

/// Expand legacy `-o value value` syntax into repeated nanoargs options.
fn normalize_config_options(args: &mut Vec<String>) {
    let mut index = 0;
    while index < args.len() {
        let is_option = matches!(args[index].as_str(), "-o" | "--option")
            || args[index].starts_with("-o=")
            || args[index].starts_with("--option=")
            || (args[index].starts_with("-o") && args[index].len() > 2);
        if !is_option {
            index += 1;
            continue;
        }

        // An option without an attached value consumes the next token itself.
        index += usize::from(matches!(args[index].as_str(), "-o" | "--option")) + 1;
        while index < args.len() && !args[index].starts_with('-') {
            args.insert(index, String::from("--option"));
            index += 2;
        }
    }
}

/// Count repeated verbosity flags, which nanoargs intentionally stores as booleans.
fn count_verbosity(args: &[String], short: char, long: &str) -> u8 {
    let mut count = 0u8;
    for arg in args {
        if arg == long {
            count = count.saturating_add(1);
            continue;
        }

        let Some(cluster) = arg.strip_prefix('-').filter(|arg| !arg.starts_with('-')) else {
            continue;
        };
        if cluster.chars().all(|flag| matches!(flag, 'q' | 'v')) {
            count =
                count.saturating_add(cluster.chars().filter(|flag| *flag == short).count() as u8);
        }
    }
    count
}

/// Parse the class CLI parameter.
fn parse_class(input: &str) -> Result<Class, String> {
    let (general, instance) = match input.split_once(',') {
        Some((_, instance)) if instance.contains(',') => {
            return Err(String::from("Too many parameters"));
        }
        Some((general, instance)) => (general, instance),
        None => (input, input),
    };

    Ok(Class::new(general, instance))
}

/// Convert to hex if possible, else decimal.
#[cfg(all(unix, not(target_os = "macos")))]
fn parse_hex_or_decimal(input: &str) -> Option<u32> {
    input
        .strip_prefix("0x")
        .and_then(|value| u32::from_str_radix(value, 16).ok())
        .or_else(|| input.parse().ok())
}

/// Terminal-specific CLI options which can be passed to new windows.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct TerminalOptions {
    /// Start the shell in the specified working directory.
    pub working_directory: Option<PathBuf>,

    /// Remain open after child process exit.
    pub hold: bool,

    /// Command and args to execute (must be last argument).
    command: Vec<String>,
}

impl TerminalOptions {
    /// Shell override passed through the CLI.
    pub fn command(&self) -> Option<Program> {
        let (program, args) = self.command.split_first()?;
        Some(Program::WithArgs {
            program: program.clone(),
            args: args.to_vec(),
        })
    }

    /// Override the [`PtyOptions`]'s fields with the [`TerminalOptions`].
    pub fn override_pty_config(&self, pty_config: &mut PtyOptions) {
        if let Some(working_directory) = &self.working_directory {
            if working_directory.is_dir() {
                pty_config.working_directory = Some(working_directory.to_owned());
            } else {
                error!("Invalid working directory: {working_directory:?}");
            }
        }

        if let Some(command) = self.command() {
            pty_config.shell = Some(command.into());
        }

        pty_config.drain_on_exit |= self.hold;
    }
}

impl From<TerminalOptions> for PtyOptions {
    fn from(mut options: TerminalOptions) -> Self {
        PtyOptions {
            working_directory: options.working_directory.take(),
            shell: options.command().map(Into::into),
            drain_on_exit: options.hold,
            env: HashMap::new(),
            #[cfg(target_os = "windows")]
            escape_args: false,
            #[cfg(target_os = "linux")]
            escape_args: false,
            #[cfg(target_os = "macos")]
            escape_args: false,
        }
    }
}

/// Window identity options.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct WindowIdentity {
    /// Defines the window title [default: Alacritty].
    pub title: Option<String>,

    /// Defines window class [default: Alacritty].
    pub class: Option<Class>,
}

impl WindowIdentity {
    /// Override the [`Identity`] fields with values from CLI.
    pub fn override_identity_config(&self, identity: &mut Identity) {
        if let Some(title) = &self.title {
            identity.title.clone_from(title);
        }

        if let Some(class) = &self.class {
            identity.class.clone_from(class);
        }
    }
}

/// Subset of options used when creating a new window in-process.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct WindowOptions {
    /// Terminal options for the new window.
    pub terminal_options: TerminalOptions,

    /// Window options for the new window.
    pub window_identity: WindowIdentity,

    /// Identifier used to group windows into tabs on macOS.
    #[cfg(target_os = "macos")]
    #[serde(default)]
    pub window_tabbing_id: Option<String>,

    /// Override configuration file options [example: 'cursor.style=\"Beam\"'].
    option: Vec<String>,
}

impl WindowOptions {
    /// Get the parsed set of CLI config overrides.
    pub fn config_overrides(&self) -> ParsedOptions {
        ParsedOptions::from_options(&self.option)
    }
}

/// Parsed CLI config overrides.
#[derive(Debug, Default)]
pub struct ParsedOptions {
    config_options: Vec<(String, Value)>,
}

impl ParsedOptions {
    /// Parse CLI config overrides.
    pub fn from_options(options: &[String]) -> Self {
        let mut config_options = Vec::new();

        for option in options {
            let parsed = match toml::from_str(option) {
                Ok(parsed) => parsed,
                Err(err) => {
                    eprintln!("Ignoring invalid CLI option '{option}': {err}");
                    continue;
                }
            };

            config_options.push((option.clone(), parsed));
        }

        Self { config_options }
    }

    /// Apply CLI config overrides, removing broken ones.
    pub fn override_config(&mut self, config: &mut UiConfig) {
        let mut i = 0;
        while i < self.config_options.len() {
            let (option, parsed) = &self.config_options[i];
            match config.replace(parsed.clone()) {
                Err(err) => {
                    error!(
                        target: LOG_TARGET_IPC_CONFIG,
                        "Unable to override option '{option}': {err}"
                    );
                    self.config_options.remove(i);
                }
                Ok(_) => i += 1,
            }
        }
    }

    /// Apply CLI config overrides to a CoW config.
    pub fn override_config_rc(&mut self, config: Rc<UiConfig>) -> Rc<UiConfig> {
        if self.config_options.is_empty() {
            return config;
        }

        let mut config = (*config).clone();
        self.override_config(&mut config);
        Rc::new(config)
    }

    /// Apply CLI config overrides to a CoW config (immutable version).
    pub fn override_config_rc_immutable(&self, config: Rc<UiConfig>) -> Rc<UiConfig> {
        if self.config_options.is_empty() {
            return config;
        }

        let mut config = (*config).clone();
        for (option, parsed) in &self.config_options {
            if let Err(err) = config.replace(parsed.clone()) {
                error!(
                    target: LOG_TARGET_IPC_CONFIG,
                    "Unable to override option '{option}': {err}"
                );
            }
        }
        Rc::new(config)
    }

    /// Append another ParsedOptions.
    pub fn append(&mut self, other: &ParsedOptions) {
        self.config_options
            .extend(other.config_options.iter().cloned());
    }

    /// Merge another ParsedOptions (same as append).
    pub fn merge(&mut self, other: &ParsedOptions) {
        self.append(other);
    }
}

impl Deref for ParsedOptions {
    type Target = Vec<(String, Value)>;

    fn deref(&self) -> &Self::Target {
        &self.config_options
    }
}

impl DerefMut for ParsedOptions {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.config_options
    }
}

/// IPC socket messages.
#[cfg(unix)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SocketMessage {
    /// Create a new window in the same Alacritty process.
    CreateWindow(WindowOptions),
    /// Update the Alacritty configuration.
    Config(IpcConfig),
    /// Read runtime Alacritty configuration.
    GetConfig(IpcGetConfig),
}

/// Parameters to the `config` IPC subcommand / message.
#[cfg(unix)]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IpcConfig {
    /// Configuration file options [example: 'cursor.style="Beam"'].
    #[serde(default)]
    pub options: Vec<String>,
    /// Window ID for the config change. Use -1 to apply to all windows.
    pub window_id: Option<i128>,
    /// Clear all runtime configuration changes.
    #[serde(default)]
    pub reset: bool,
}

/// Parameters to the `get-config` IPC message.
#[cfg(unix)]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IpcGetConfig {
    /// Window ID for the config request. Use -1 for global config.
    pub window_id: Option<i128>,
}

/// Socket reply types.
#[cfg(unix)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SocketReply {
    /// Config response.
    GetConfig(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use toml::Table;

    fn args(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn parses_repeated_verbosity_and_command_tail() {
        let options = Options::parse(args(&["-vvv", "-e", "cmd", "/c", "--child-flag"])).unwrap();

        assert_eq!(options.log_level(), LevelFilter::Trace);
        assert_eq!(
            options.window_options.terminal_options.command,
            ["cmd", "/c", "--child-flag"]
        );
    }

    #[test]
    fn parses_quiet_levels() {
        let error = Options::parse(args(&["-q"])).unwrap();
        let off = Options::parse(args(&["-qq"])).unwrap();

        assert_eq!(error.log_level(), LevelFilter::Error);
        assert_eq!(off.log_level(), LevelFilter::Off);
    }

    #[test]
    fn rejects_mixed_verbosity() {
        assert!(Options::parse(args(&["-vq"])).is_err());
    }

    #[test]
    fn parses_title_alias_and_class() {
        let options =
            Options::parse(args(&["-t", "Terminal", "--class", "General,Instance"])).unwrap();

        assert_eq!(
            options.window_options.window_identity.title.as_deref(),
            Some("Terminal")
        );
        let class = options.window_options.window_identity.class.unwrap();
        assert_eq!(class.general, "General");
        assert_eq!(class.instance, "Instance");
    }

    #[test]
    fn parses_repeated_config_overrides() {
        let options =
            Options::parse(args(&["-o", "cursor.style='Beam'", "window.opacity=0.8"])).unwrap();

        assert_eq!(options.window_options.option.len(), 2);
        assert_eq!(options.config_options.len(), 2);
    }

    #[test]
    fn handles_builtin_output_and_missing_command() {
        assert!(matches!(
            Options::parse(args(&["--help"])),
            Err(CliError::Parse(ParseError::HelpRequested(_)))
        ));
        assert!(matches!(
            Options::parse(args(&["--version"])),
            Err(CliError::Parse(ParseError::VersionRequested(_)))
        ));
        assert!(Options::parse(args(&["-e"])).is_err());
    }

    #[test]
    fn dynamic_title_ignoring_options_by_default() {
        let mut config = UiConfig::default();
        let old_dynamic_title = config.window.dynamic_title;

        Options::default().override_config(&mut config);

        assert_eq!(old_dynamic_title, config.window.dynamic_title);
    }

    #[test]
    fn dynamic_title_not_overridden_by_config() {
        let mut config = UiConfig::default();

        config.window.identity.title = "foo".to_owned();
        Options::default().override_config(&mut config);

        assert!(config.window.dynamic_title);
    }

    #[test]
    fn valid_option_as_value() {
        let value: Value = toml::from_str("field=true").unwrap();

        let mut table = Table::new();
        table.insert(String::from("field"), Value::Boolean(true));

        assert_eq!(value, Value::Table(table));

        let value: Value = toml::from_str("parent.field=true").unwrap();

        let mut parent_table = Table::new();
        parent_table.insert(String::from("field"), Value::Boolean(true));
        let mut table = Table::new();
        table.insert(String::from("parent"), Value::Table(parent_table));

        assert_eq!(value, Value::Table(table));
    }

    #[test]
    fn invalid_option_as_value() {
        let value = toml::from_str::<Value>("}");
        assert!(value.is_err());
    }

    #[test]
    fn float_option_as_value() {
        let value: Value = toml::from_str("float=3.4").unwrap();

        let mut expected = Table::new();
        expected.insert(String::from("float"), Value::Float(3.4));

        assert_eq!(value, Value::Table(expected));
    }

    #[test]
    fn parse_instance_class() {
        let class = parse_class("one").unwrap();
        assert_eq!(class.general, "one");
        assert_eq!(class.instance, "one");
    }

    #[test]
    fn parse_general_class() {
        let class = parse_class("one,two").unwrap();
        assert_eq!(class.general, "one");
        assert_eq!(class.instance, "two");
    }

    #[test]
    fn parse_invalid_class() {
        let class = parse_class("one,two,three");
        assert!(class.is_err());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn valid_decimal() {
        let value = parse_hex_or_decimal("10485773");
        assert_eq!(value, Some(10485773));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn valid_hex_to_decimal() {
        let value = parse_hex_or_decimal("0xa0000d");
        assert_eq!(value, Some(10485773));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn invalid_hex_to_decimal() {
        let value = parse_hex_or_decimal("0xa0xx0d");
        assert_eq!(value, None);
    }
}
