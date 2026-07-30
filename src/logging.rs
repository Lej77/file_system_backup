use std::{
    io::{self, Write},
    sync::RwLock,
};

use clap::{ArgAction, Args};
use indicatif::{ProgressBar, WeakProgressBar};

#[derive(Debug, Args, Clone)]
pub struct CommonOpt {
    /// Provide more verbose logging. Can be specified up to 2 times to increase
    /// verbosity level.
    #[clap(short, long, action = ArgAction::Count, help_heading = "LOGGING")]
    pub verbose: u8,
    /// Quiet mode, suppresses some logging. Specify once to only show warnings
    /// and errors. If you specify it 3 times then all logging will be suppressed
    /// but note that if the program exits with an error info about that will still
    /// be written to stderr.
    #[clap(
        short,
        long,
        action = ArgAction::Count,
        conflicts_with = "verbose",
        help_heading = "LOGGING"
    )]
    pub quiet: u8,
}
impl CommonOpt {
    /// Enable logging based on specified verbosity arguments.
    pub fn configure_logging(&self) {
        let verbosity_level_number = 3_i32 - (self.quiet as i32) + (self.verbose as i32);
        let verbosity_level = verbosity_level(verbosity_level_number.max(0) as u32);
        init_logger(verbosity_level);

        if verbosity_level_number < 0 {
            // You normally won't see this, but it can probably be enabled via environment variables:
            log::warn!(
                "Specified logging level {} but 0 is the lowest level",
                verbosity_level_number
            )
        }
        if verbosity_level_number > 5 {
            log::warn!(
                "Specified logging level {} but 5 is the highest level",
                verbosity_level_number
            )
        }
        log::info!(
            "Logging with verbosity level: {} - {}",
            verbosity_level_number.min(5),
            verbosity_level
                .map(|level| level.to_string())
                .unwrap_or_else(|| "Off".to_string())
        );
    }
}

pub fn verbosity_level(verbose: u32) -> Option<log::Level> {
    use log::Level::*;
    Some(match verbose {
        0 => return None,
        1 => Error,
        2 => Warn,
        3 => Info,
        4 => Debug,
        _ => Trace,
    })
}

static CURRENT_PROGRESS_BAR: RwLock<Option<WeakProgressBar>> = RwLock::new(None);

pub fn set_progress_bar<'a>(pb: impl Into<Option<&'a ProgressBar>>) {
    let pb = pb
        .into()
        // Don't care about hidden progress bars:
        .filter(|pb| !pb.is_hidden())
        .map(|pb| pb.downgrade());
    *CURRENT_PROGRESS_BAR.write().unwrap() = pb;
}
pub fn get_progress_bar() -> Option<ProgressBar> {
    CURRENT_PROGRESS_BAR.read().unwrap().as_ref()?.upgrade()
}

pub fn init_logger(default_level: Option<log::Level>) {
    use chrono::Local;
    use env_logger::{Builder, Env};
    use log::Level;

    let default_filter = if let Some(default_level) = default_level {
        if default_level == Level::Trace {
            "trace\
                    ,dav_server=debug\
                    ,xml=debug\
                    ,file_system_backup::mount::web_dav_mount=debug"
                .to_lowercase()
        } else if default_level == Level::Debug {
            format!(
                "{:?}\
                    ,dav_server=info\
                    ,xml=info\
                    ,mft::mft=info",
                default_level
            )
            .to_lowercase()
        } else {
            format!("{default_level:?}").to_lowercase()
        }
    } else {
        "off".to_string()
    };
    let mut builder = Builder::from_env(Env::new().default_filter_or(default_filter.as_str()));

    builder.format(|formatter, record| {
        let pg = get_progress_bar().filter(|pb| !pb.is_hidden() && !pb.is_finished());
        let mut msg = Vec::new();
        struct Wrapper<T1, T2> {
            first: Option<T1>,
            second: T2,
        }
        impl<T1, T2> Write for Wrapper<T1, T2>
        where
            T1: Write,
            T2: Write,
        {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if let Some(first) = &mut self.first {
                    first.write(buf)
                } else {
                    self.second.write(buf)
                }
            }

            fn flush(&mut self) -> io::Result<()> {
                if let Some(first) = &mut self.first {
                    first.flush()
                } else {
                    self.second.flush()
                }
            }
        }

        let mut wrapper = Wrapper {
            first: pg.is_some().then_some(&mut msg),
            second: formatter,
        };

        let level_style = wrapper.second.default_level_style(record.level());
        let bold_style = env_logger::fmt::style::Style::new().bold();

        writeln!(
            wrapper,
            " {} {level_style}[{}]{level_style:#}{} {bold_style}({}){bold_style:#}: {}",
            Local::now().format("%Y-%m-%d %H:%M:%S"),
            record.level(),
            match record.level() {
                Level::Debug | Level::Error | Level::Trace => "",
                Level::Info | Level::Warn => " ",
            },
            record.target(),
            record.args()
        )?;

        if let Some(pg) = pg {
            pg.println(std::str::from_utf8(&msg).expect("log message should be UTF8"));
        }

        Ok(())
    });

    builder.init();

    log::trace!(
        "Default log filter at this level (used if RUST_LOG is not specified): {default_filter}"
    );
}
