//! `bh3` - command line interface of Bonsai H3 Writer (see `docs/CLI.md`).

fn main() {
    std::process::exit(bh3::cli::run(std::env::args_os()));
}
