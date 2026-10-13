mod account;
mod bench;
mod collect;
mod coverage;
mod install;
mod link;
mod poll;
mod report;
mod share;
mod store;

const USAGE: &str = "\
ai-usage: token, quota and speed ledger for Claude Code and Codex

usage:
  ai-usage [-n N] [--tool claude|codex]   quota windows, speed and today's totals
  ai-usage tps [--days N] [--tool T] [--hourly]
  ai-usage install [--dry-run] [--force] [--no-claude] [--no-codex] [--no-service]
  ai-usage uninstall [--dry-run] [--no-claude] [--no-codex] [--no-service]
  ai-usage login                           join the community data with GitHub
  ai-usage profile [--anonymous|--public]  how you appear on the contributor list
  ai-usage share [--dry-run]               upload finished speed points now
  ai-usage link [--rotate] [--dry-run]     your private page link
  ai-usage logout                          stop uploading and forget the key
  ai-usage serve                           OTLP receiver + 5-minute quota sampler
  ai-usage poll                            take one quota sample now
  ai-usage bench [--tool T] [--model M]    one speed benchmark round (spends quota)
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or_default();
    let result = match args.first().map(String::as_str) {
        Some("serve") => collect::serve(),
        Some("poll") => {
            poll::poll_once();
            Ok(())
        }
        Some("tps") => report::tps(rest),
        Some("install") => install::install(rest),
        Some("uninstall") => install::uninstall(rest),
        Some("bench") => bench::run(rest),
        Some("login") => account::login(rest),
        Some("logout") => account::logout(),
        Some("profile") => account::profile(rest),
        Some("share") => share::run(rest),
        Some("link") => link::run(rest),
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            Ok(())
        }
        Some("-V" | "--version") => {
            println!("ai-usage {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => report::summary(&args),
    };
    if let Err(e) = result {
        eprintln!("ai-usage: {e}");
        std::process::exit(1);
    }
}
