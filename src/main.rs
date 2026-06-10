// Copyright (c) 2022 Lev Kokotov <hi@levthe.dev>
// Copyright (c) 2023 Dmitriy Vasiliev <dmitrivasilyev@ozon.ru>

// Permission is hereby granted, free of charge, to any person obtaining
// a copy of this software and associated documentation files (the
// "Software"), to deal in the Software without restriction, including
// without limitation the rights to use, copy, modify, merge, publish,
// distribute, sublicense, and/or sell copies of the Software, and to
// permit persons to whom the Software is furnished to do so, subject to
// the following conditions:

// The above copyright notice and this permission notice shall be
// included in all copies or substantial portions of the Software.

// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
// EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
// NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE
// LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION
// WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

use tikv_jemallocator::Jemalloc;
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

extern crate exitcode;

use pg_doorman::app;

fn main() {
    app::install_panic_hook();

    if let Err(err) = run() {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = app::parse_args()?;
    app::cleanup_inherited_upgrade_fds(&args);
    let config = app::init_config(&args)?;

    if args.test_config {
        println!(
            "pg_doorman: the configuration file {} syntax is ok",
            args.config_file
        );
        println!(
            "pg_doorman: configuration file {} test is successful",
            args.config_file
        );
        return Ok(());
    }

    app::init_logging(&args, &config)?;
    // apply TOML `[general] log_level` override on top of
    // the CLI / env default the logger was just built with. Silently
    // ignore parse errors and log them at warn-level - a malformed
    // TOML value should not block startup.
    if let Some(level) = config.general.log_level.as_deref() {
        if let Err(err) = app::log_level::set_log_level(level) {
            log::warn!(
                "[general] log_level = {level:?} failed to apply: {err}; \
                 keeping CLI default"
            );
        }
    }
    app::run_server(args, config)
}
