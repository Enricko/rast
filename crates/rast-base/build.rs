//! Re-run the build when a migration is added or changed: `sqlx::migrate!`
//! embeds the migrations at compile time and does not always notice a new file.

fn main() {
    println!("cargo:rerun-if-changed=../../migrations");
}
