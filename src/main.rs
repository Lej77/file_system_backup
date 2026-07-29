use clap::Parser;
use color_eyre::eyre::Context;
use file_system_backup::*;

fn main() -> Result<()> {
    color_eyre::install()?;

    if Some(CleanupProcess::get_cleanup_argument()) == std::env::args_os().nth(1) {
        return CleanupProcess::handle_cleanup();
    }
    let opts = Opts::parse();

    opts.configure_logging();
    log::trace!("Parsed arguments:\n{:#?}\n", opts);
    log::trace!(
        "Unparsed Args: {:?}",
        std::env::args_os().collect::<Vec<_>>()
    );

    let should_quit = CancelSignal::new();
    ctrlc::set_handler({
        let should_quit = should_quit.clone();
        let mut second_signal = false;
        move || {
            should_quit.cancel_with_reason("Ctrl-C signal");
            if second_signal {
                eprintln!("Received second Ctrl-C signal, terminating current process immediately");
                std::process::exit(100);
            } else {
                log::info!("Received Ctrl-C signal, cancelling all operations");
            }
            second_signal = true;
        }
    })
    .wrap_err("Error setting Ctrl-C handler")?;

    add_backtrace_note_to_error(opts.run(&should_quit))
}
