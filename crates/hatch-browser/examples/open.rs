//! Read-only smoke test: open a URL in a throwaway profile and print what the
//! assistant would see.
//!
//! `cargo run -p hatch-browser --example open -- https://example.com`

use hatch_browser::{Broker, BrowserConfig};

fn main() {
    let url = std::env::args().nth(1).unwrap_or_else(|| "https://example.com".into());
    let dir = std::env::temp_dir().join(format!("hatch-browser-smoke-{}", std::process::id()));
    let broker = Broker::new(BrowserConfig::new(&dir));
    let db = hatch_store::Db::open_in_memory().expect("db");
    let started = std::time::Instant::now();
    match broker.open(&db, "chat:smoke", &url, None) {
        Ok(v) => println!("{}\n\n（{:.1} 秒）", v.render(), started.elapsed().as_secs_f32()),
        Err(e) => println!("{e}"),
    }
    drop(broker);
    let _ = std::fs::remove_dir_all(&dir);
}
