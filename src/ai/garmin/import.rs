//! Run the app's own Garmin history import against a *copy* of a database.
//!
//! A harness, not a test: it calls the live API and writes to whatever
//! database `CYCLE_DATA_HOME` names, so it is `#[ignore]`d. It refuses to run
//! unless that variable is set and names a directory other than the real ones
//! (both called `cycle`), because a `--release` test build would otherwise
//! resolve to the rider's own data.
//!
//! It is [`sync::import_history_live`] — what the Import History button runs —
//! with its progress printed, so a run here is a run of the production path.
//!
//! ```text
//! sqlite3 <live cycle.db> ".backup ~/.cache/cycle-garmin-review/cycle.db"
//! CYCLE_DATA_HOME=~/.cache/cycle-garmin-review \
//!   cargo test --release garmin_import_all -- --ignored --nocapture
//! ```

use super::sync;
use crate::data::{db, keystore, paths};
use std::time::Instant;

#[tokio::test]
#[ignore = "calls the live Garmin API and writes to the CYCLE_DATA_HOME database"]
async fn garmin_import_all() {
    assert!(
        std::env::var_os(paths::DATA_HOME_ENV).is_some(),
        "set {} to the directory holding the copy",
        paths::DATA_HOME_ENV
    );
    let dir = paths::data_dir();
    assert_ne!(
        dir.file_name().and_then(|n| n.to_str()),
        Some("cycle"),
        "refusing to import into {} — that is a real data directory",
        dir.display()
    );
    let pool = db::open().await.expect("open the copy");
    let key = keystore::get_secret(keystore::KEY_GARMIN_API)
        .unwrap()
        .expect("no Garmin key");
    let guard = sync::try_begin_import().expect("import slot");
    let started = Instant::now();
    let last = std::sync::Mutex::new(String::new());
    let report = sync::import_history_live(
        &pool,
        &key,
        chrono::Local::now().date_naive(),
        &guard,
        |p| {
            // One line per stage, and every 50th file.
            let line = match p {
                sync::Progress::Wellness { .. } => "wellness".to_string(),
                sync::Progress::Activities => "activities".to_string(),
                sync::Progress::Files { done, total } => {
                    format!("files {}/{total}", done - done % 50)
                }
            };
            let mut last = last.lock().unwrap();
            if *last != line {
                println!("{:>5.0} s  {line}", started.elapsed().as_secs_f32());
                *last = line;
            }
        },
    )
    .await
    .expect("import");
    println!("{report:#?}");
    println!("Total {:.0} s", started.elapsed().as_secs_f32());
}
